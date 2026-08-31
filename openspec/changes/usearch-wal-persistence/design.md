# Design: usearch-wal-persistence

**Источник истины по архитектуре: `docs/adr/0004-usearch-lsm-segments.md`** (аудит текущей реализации, факты о usearch 2.26, все решения с причинами и отклонёнными альтернативами). Этот файл — сводка, синхронизированная с ADR 0004 (ревизия 2026-08-31: первоначальный дизайн «глобального кумулятивного WAL» отклонён ADR 0004).

## Key Decisions (Human 2026-08-31 + ADR 0004)

| # | Решение | Причина (почему не альтернатива) |
|---|---|---|
| D1 | WAL в SQLite, per-segment, **только DEL** (2) | человек: транзационная целостность; DEL-only: WAL остаётся маленьким, сегмент самодостаточен, кумулятивные `segment_id <= n` запросы не нужны (supersession пишет `(s,k,DEL)` в каждый старший сегмент атомарно на операцию) |
| D2 | Манифест ключей — **sidecar `.keys`** (magic+count+u32[]) | не раздувает WAL до размера корпуса; `count()`/`chunk_ids()`/компакция без `exact_search`-сканов (API перечисления у usearch нет); переживает краши (sidecar атомарный tmp+rename) |
| D3 | RAM = `restore_from_buffer` (копия в памяти) + явные `save()` (flush/shutdown) | эксперимент: `add` через mmap-`restore` **не персистит** в файл — ложная персистентность; DISK[n] = `restore_view` (read-only mmap, zero-copy) |
| D4 | Идентификаторы сегментов **монотонные, без перенумерации** | перенумерация при компакции даёт алиасинг WAL-строк в краш-окне; монотонные id: новые сегменты N+1.., старым строкам не за что «прилипнуть» |
| D5 | Flush при `RAM.size() ≥ max_segment_vectors`: save → segment-n, WAL segment 0 очистить, RAM reset | переполнение по числу векторов (человек); `DELETE segment_id=0` корректно: DEL-строки — только для ключей, физически отсутствующих в flushed-файле |
| D6 | Поиск: параллельно по слоям (rayon pool = `search_threads`), `filtered_search` с кэшем stale-множеств (версия `AtomicU64`), merge «свежий слой выигрывает» | ноль SQL в стационарном режиме (гейт p95 ADR 0003); дубликаты возможны только в краш-окне — свежий слой = корректная версия |
| D7 | Компакция: фон (`std::thread`), триггер `stale/total > compaction_stale_threshold %`, смена каталога `segments/` атомарным rename, WAL-очистка **после** смены | порядок «каталог → WAL» обязателен: обратный даёт «озивание» удалённых ключей; поиск не блокируется (Arc-клоны старых сегментов) |
| D8 | Трейт `VectorIndex`: аддитивный `maybe_compact()` с default no-op | `&mut self` несовместим с `Arc<dyn VectorIndex>`; аддитивный метод не ломает существующие имплементации (Lance — no-op) |
| D9 | `rebuild` = полный сброс: WAL очистить, файлы DISK удалить, RAM = rows | ultimate repair не должен оставлять старые слои (дефект текущей реализации) |

## Architecture (схема, детали — ADR 0004 §1–§9)

```
<vectors_path>/usearch/
├── ram.usearch + ram.keys            # RAM: копия в памяти, save при flush/shutdown
└── segments/
    ├── segment-1.usearch + .keys     # DISK[n]: restore_view (read-only mmap)
    └── ...                           # id монотонны: 0=RAM (свежий), N+1 = новый flush/компакция

SQLite usearch_vectors_log (схема без изменений, миграции 3+4):
    PK (segment_id, chunk_id), flags=DEL(2) только
    invariant: supersession/удаление → одна транзакция → (s,k,DEL) во все старшие сегменты, WAL-first
```

- **Write:** `insert_batch` → WAL-транзакция supersession → add в RAM → при переполнении flush (D5). `delete_by_chunk_ids` → WAL-транзакция (`(0,k,DEL)` + старшие) → remove из RAM.
- **Search:** кэш stale (bump версии на запись) → `filtered_search` per layer параллельно → merge (свежий слой, distance asc, truncate k). `count`/`chunk_ids` — по `ram_keys`/sidecar − stale (без сканов).
- **Compaction (D7):** live = `keys(n) − stale[n]`; сборка `get(key)`; новые сегменты в `segments.tmp/` (новые id); rename-смена; `DELETE segment_id > 0`.
- **Open/восстановление:** мусор (`segments.tmp/.old`, `*.tmp`) → скан `segments/` → RAM `restore_from_buffer` → WAL reconciliation (удалить строки несуществующих сегментов) → dim-check → кэш stale.
- **Wiring:** фабрика `create_vector_engine` получает путь к knowledge.db (engine открывает посвящённое `rusqlite::Connection` для WAL) + `VectorIndexConfig.usearch`; `maybe_compact()` — в ingestion-cleanup после batch; `build_index()` — при graceful shutdown serve.

## Crash-семантика (честно, ADR 0004 §5, §7)

- Окно потери: вставки RAM с последнего flush/shutdown-save (≤ `max_segment_vectors`) при краше. WAL без векторов не реплеит вставки; repair — consumer-реконсиляция (SQLite − index → re-ingest) / `rebuild`. Осознанное ограничение «WAL without vectors».
- Все краш-окна компакции/flush/self-healing — в ADR 0004 §7 (порядки операций фиксированы).

## Files Modified

- `crates/vectors/src/usearch_engine.rs` — полная переработка (layout, WAL, search, flush, compaction)
- `crates/vectors/src/lib.rs` — трейт `maybe_compact()` (аддитивный), фабрика (+WAL-путь, +UsearchConfig)
- `crates/db/src/connection.rs`, `crates/db/src/test_util.rs` — фикс тестов (user_version 4, segment_id)
- `crates/cli/src/serve/bootstrap.rs`, `crates/cli/src/serve/server.rs` — wiring (WAL-путь, shutdown-save)
- `crates/ingestion/src/runner/cleanup.rs` — вызов `maybe_compact()`
- `crates/config/src/preset.rs` — без изменений (секция `vectors.usearch` уже реализована)

## Non-goals

- Изменение контракта VectorIndex (кроме аддитивного `maybe_compact`, D8)
- Изменение LanceEngine
- MCP tool surface
- Изменение схемы `usearch_vectors_log` и секции конфига `vectors.usearch`
- Replay векторов из WAL (D: «WAL without vectors»)

## Go-oracle reference

Нет соответствия в оракуле: vec0 store в `../synopsis/internal/database` не имел WAL/сегментации (Go-оригинал хранил векторы в SQLite vec0 с полным пересчётом). Новый операционный механизм — референс только по контракту `VectorIndex` (behavior: cascade protocol design D3 крейта `vectors`).
