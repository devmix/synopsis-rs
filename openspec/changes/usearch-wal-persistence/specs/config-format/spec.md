# config-format Specification (delta)

## ADDED Requirements

### Requirement: Секция vectors.wal

Секция `vectors:` дополняется опциональным объектом `wal`, содержащим параметры persistence для UsearchEngine. Поле необязательно: при отсутствии применяются дефолты (512 MB, 5 слоёв, 50% WAL). Невалидные значения вызывают ошибку парсинга/валидации.

#### Scenario: Отсутствие секции wal
- **WHEN** пресет содержит секцию `vectors` без поля `wal`
- **THEN** применяются дефолты: ram_threshold_mb=512, disk_count_threshold=5, wal_size_threshold_pct=50

#### Scenario: Явная настройка wal
- **WHEN** пресет задаёт `vectors.wal.ram_threshold_mb: 1024`
- **THEN** используется значение 1024 MB для порога сброса RAM

#### Scenario: Невалидное значение
- **WHEN** пресет задаёт `vectors.wal.ram_threshold_mb: 0`
- **THEN** конфигурация отклоняется с явной ошибкой

#### Scenario: Частичная настройка
- **WHEN** пресет задаёт `vectors.wal.disk_count_threshold: 10` без остальных полей
- **THEN** `disk_count_threshold=10`, остальные — дефолты (512, 50)
