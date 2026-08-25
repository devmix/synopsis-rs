# Tasks: search-module

Зависимости change'а: crates/db (search_fts, DAOs), crates/vectors (engine search),
crates/embedding (trait), crates/graph (traverser), crates/config (SearchConfig/GraphConfig) — все готовы.
Оракул (read-only): `../synopsis/internal/search/*.go` + тесты в том же каталоге.
Дизайн: design.md D1–D9. Гейты каждой задачи: `cargo fmt --check`,
`cargo clippy -p <crate> --all-targets -- -D warnings`, `cargo test -p <crate>`;
`cargo test --workspace` и cargo doc гоняет ТОЛЬКО оркестратор.
Директива (binding): НЕ копировать Go 1:1 — функциональная копия, архитектура для Rust
(DRY/KISS/SOLID/YAGNI); баги оракула исправлять или фиксировать отклонения.

- [ ] 4.1 Скаффолдинг крейта + типы + RRF
  - Цель: новый крейт crates/search и ядро фьюжна.
  - Scope файлов: `crates/search/{Cargo.toml,src/lib.rs,src/error.rs,src/rrf.rs}` (новые),
    корневой Cargo.toml (+workspace member).
  - Содержание: SearchError (thiserror), SearchResult{chunk_id, chunk_text, document_id,
    sequence_num, start/end_offset, document_path, score, rank, source_type, metadata,
    entities}, сырые LexicalHit/SemanticHit; ReciprocalRankFusion по design D4
    (k default 20, min-max BM25 только по лексическим записям с нейтралью 0.5,
    RRF-нормализация, 0.7/0.3 калибровка, tiebreak chunk_id asc, topN<=0 без усечения).
  - Тесты: ПОЛНЫЙ паритет `../synopsis/internal/search/rrf_test.go`.
  - Критерии приёмки: гейты search зелёные.

- [ ] 4.2 Суб-поиски: лексический и семантический
  - Цель: две ноги поиска.
  - Scope файлов: `crates/search/src/lexical.rs`, `crates/search/src/semantic.rs` (новые),
    `crates/search/Cargo.toml` (+db, embedding, vectors workspace).
  - Содержание: lexical — обёртка ChunkDao::search_fts (domain Option<&str>, FtsHit →
    LexicalHit); semantic — embed запроса (пустой эмбеддинг = ошибка) → VectorIndex::search
    → батч-резолв чанков через ChunkDao (get_by_ids или list-аналог — проверить API) →
    доменный фильтр на стороне приложения с OVERFETCH_FACTOR=3 (design D3: резолв
    document domains из метаданных документов) → truncate topK. Пустой запрос → пусто.
  - Тесты: стабы DAO/эмбеддера/индекса: happy path обеих ног; over-fetch фильтрация
    (домен выживает, чужой отсекается); mismatch размерности; пустые входы.
  - Критерии приёмки: гейты search зелёные.

- [ ] 4.3 Enricher + batch-метод в db
  - Цель: обогащение результатов.
  - Scope файлов: `crates/search/src/enrich.rs` (новый),
    `crates/db/src/chunk_entity.rs` (+get_entities_by_chunks батч),
    `crates/search/Cargo.toml`.
  - Содержание: design D6 — батч DocumentDao::get_by_ids + новый батчевый
    get_entities_by_chunks (IN-листы как в существующих DAO); merge source_type
    ("lexical+pdf"), normalize_updated_at (RFC3339 | SQLite layout | fractional → RFC3339),
    extract_reranker_flags, normalize_domains (string | array → Vec<String>),
    entities из батч-карты. Полный возврат enriched пула.
  - Тесты: паритет `enricher_test.go` + юнит-тесты нового db-метода (in-memory SQLite).
  - Критерии приёмки: гейты search И db зелёные.

- [ ] 4.4 Reranker
  - Цель: бизнес-правила и бусты.
  - Scope файлов: `crates/search/src/rerank.rs` (новый).
  - Содержание: design D7 — дефолты 0.2/1.5/1.2/90, конфиг-оверрайды только >0,
    authority_boost map; business rules (deprecated/official/expired компуются),
    freshness по updated_at из enriched metadata (RFC3339), authority по
    document_source_type; re-sort desc + перенумерация рангов.
  - Тесты: полный паритет `reranker_test.go`.
  - Критерии приёмки: гейты search зелёные.

- [ ] 4.5 Graph expander
  - Цель: расширение сущностей графом.
  - Scope файлов: `crates/search/src/expand.rs` (новый), `crates/search/Cargo.toml` (+graph).
  - Содержание: design D8 — для сущностей результата, присутствующих в графе:
    BFS обе стороны (max_depth/max_nodes из GraphConfig) через graph traverser,
    батч одобренных фактов FactDao::list_by_entity_ids, сериализация edges/facts в
    metadata["related_entities"]; ошибки → warn-семантика (результаты без контекста).
  - Тесты: паритет ключевых сценариев `graph_expansion_test.go` (BFS глубина/лимиты,
    facts serialization, non-fatal failure, empty graph/no entities).
  - Критерии приёмки: гейты search зелёные.

- [ ] 4.6 HybridSearcher
  - Цель: оркестрация.
  - Scope файлов: `crates/search/src/hybrid.rs` (новый), `crates/search/src/lib.rs`
    (Searcher trait / сборка).
  - Содержание: design D2/D5/D9 — последовательные суб-поиски (отклонение зафиксировано);
    both-fail → SearchError с обеими причинами; one-fail → деградация; RRF fuse
    (fusion pool = max(lexical_top_k, semantic_top_k)); finalize: enrich → rerank →
    truncate topK → expand (non-fatal). topK<=0 → дефолты из конфига. EnableLexical/
    EnableSemantic флаги. Конструктор принимает коллабораторы инъекцией (DAO-хэндлы,
    &dyn EmbeddingProvider, &dyn VectorIndex, Option<GraphExpander>).
  - Тесты: стабы: both-fail error, one-fail degrade, fusion pool, finalize порядок
    (enrich→rerank→truncate→expand), enable/disable флаги, empty query.
  - Критерии приёмки: гейты search зелёные.

- [ ] 4.7 Интеграционный тест + финальная сборка
  - Цель: сквозная проверка и публичный API.
  - Scope файлов: `crates/search/tests/hybrid_integration.rs` (новый),
    `crates/search/src/lib.rs`.
  - Содержание: in-memory SQLite с реальной FTS5 (паттерн crates/db tests) +
    MockEmbedding + MemoryIndex: индексация нескольких документов разных доменов →
    lexical/semantic/hybrid прогоны → ассерты порядка и обогащения; доменный фильтр;
    реранкер влияет на порядок; ре-экспорт публичного API из корня крейта, crate docs,
    compile-time root-API тест.
  - Критерии приёмки: `RUSTDOCFLAGS="-D warnings" cargo doc -p search --no-deps` чисто;
    гейты зелёные; workspace-тест прогоняет оркестратор.
