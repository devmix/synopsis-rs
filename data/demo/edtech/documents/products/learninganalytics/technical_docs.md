# Learning Analytics - Technical Documentation

## Architecture Overview

The Learning Analytics system processes raw user events into actionable insights through a multi-stage pipeline.

## System Components

### Event Collector
- **Purpose:** Capture user interactions
- **Technology:** Go, HTTP server
- **Responsibilities:**
  - Receive event data from clients
  - Validate event schema
  - Batch and forward to processing queue

### Event Processor
- **Purpose:** Transform raw events into metrics
- **Technology:** Go, stream processing
- **Responsibilities:**
  - Event enrichment
  - Real-time aggregation
  - Anomaly detection
  - Metric computation

### Analytics Engine
- **Purpose:** Compute complex analytics
- **Technology:** Go, SQLite with window functions
- **Responsibilities:**
  - Cohort analysis
  - Trend computation
  - Predictive modeling
  - Report generation

### Dashboard API
- **Purpose:** Serve analytics data
- **Technology:** Go, REST API
- **Responsibilities:**
  - Query aggregation
  - Data caching
  - Response formatting
  - Access control

## Event Pipeline

### Data Flow
```
Client → Event Collector → Message Queue → Event Processor
                                                   ↓
                                            Real-time Metrics
                                                   ↓
                                            Analytics Engine
                                                   ↓
                                            Aggregated Data Store
                                                   ↓
                                            Dashboard API → Client
```

### Event Schema
```json
{
    "event_id": "uuid",
    "user_id": "user_123",
    "course_id": "course_456",
    "event_type": "lesson_completed",
    "timestamp": "2024-01-15T10:30:00Z",
    "properties": {
        "lesson_id": "lesson_789",
        "duration_seconds": 1200,
        "score": 0.85,
        "device": "desktop",
        "browser": "chrome"
    },
    "context": {
        "session_id": "session_abc",
        "ip_address": "192.168.1.1",
        "user_agent": "Mozilla/5.0..."
    }
}
```

### Supported Event Types
| Event Type | Description | Properties |
|------------|-------------|------------|
| course_enrolled | User enrolled in course | course_id, source |
| lesson_started | User started a lesson | lesson_id, position |
| lesson_completed | User completed a lesson | lesson_id, duration, score |
| quiz_started | User started a quiz | quiz_id |
| quiz_completed | User completed a quiz | quiz_id, score, attempts |
| assignment_submitted | User submitted assignment | assignment_id, score |
| video_played | User played video | video_id, duration |
| resource_downloaded | User downloaded resource | resource_id |

## Database Schema

### Raw Events Table
```sql
CREATE TABLE raw_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT UNIQUE NOT NULL,
    user_id TEXT NOT NULL,
    course_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    processed INTEGER DEFAULT 0
);

CREATE INDEX idx_raw_events_user ON raw_events(user_id);
CREATE INDEX idx_raw_events_course ON raw_events(course_id);
CREATE INDEX idx_raw_events_type ON raw_events(event_type);
CREATE INDEX idx_raw_events_created ON raw_events(created_at);
```

### Aggregated Metrics Table
```sql
CREATE TABLE aggregated_metrics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    course_id TEXT NOT NULL,
    metric_date TEXT NOT NULL,
    metric_type TEXT NOT NULL,
    metric_value REAL NOT NULL,
    dimension TEXT,
    dimension_value TEXT,
    calculated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(course_id, metric_date, metric_type, dimension, dimension_value)
);

CREATE INDEX idx_metrics_course ON aggregated_metrics(course_id);
CREATE INDEX idx_metrics_date ON aggregated_metrics(metric_date);
CREATE INDEX idx_metrics_type ON aggregated_metrics(metric_type);
```

### Progress Snapshots Table
```sql
CREATE TABLE progress_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL,
    course_id TEXT NOT NULL,
    snapshot_date TEXT NOT NULL,
    total_lessons INTEGER,
    completed_lessons INTEGER,
    progress_percent REAL,
    time_spent_seconds INTEGER,
    avg_score REAL,
    last_activity TEXT,
    UNIQUE(user_id, course_id, snapshot_date)
);

CREATE INDEX idx_progress_user ON progress_snapshots(user_id);
CREATE INDEX idx_progress_course ON progress_snapshots(course_id);
CREATE INDEX idx_progress_date ON progress_snapshots(snapshot_date);
```

## Metric Computation

### Enrollment Metrics
```sql
-- Daily new enrollments
SELECT 
    date(created_at) as date,
    COUNT(DISTINCT user_id) as new_enrollments
FROM raw_events
WHERE event_type = 'course_enrolled'
GROUP BY date(created_at);

-- Total active students
SELECT 
    COUNT(DISTINCT user_id) as active_students
FROM raw_events
WHERE event_type = 'lesson_started'
AND created_at >= datetime('now', '-7 days');
```

### Engagement Metrics
```sql
-- Average session duration
SELECT 
    AVG(duration_seconds) as avg_session_duration
FROM raw_events
WHERE event_type = 'lesson_completed'
AND created_at >= datetime('now', '-30 days');

-- Lesson completion rate
SELECT 
    COUNT(CASE WHEN event_type = 'lesson_completed' THEN 1 END) * 1.0 /
    COUNT(CASE WHEN event_type = 'lesson_started' THEN 1 END) as completion_rate
FROM raw_events
WHERE course_id = ?
AND created_at >= datetime('now', '-30 days');
```

### Performance Metrics
```sql
-- Average quiz score by lesson
SELECT 
    json_extract(event_data, '$.lesson_id') as lesson_id,
    AVG(json_extract(event_data, '$.score')) as avg_score
FROM raw_events
WHERE event_type = 'quiz_completed'
GROUP BY lesson_id;

-- Student performance trend
SELECT 
    date(created_at) as date,
    AVG(json_extract(event_data, '$.score')) as avg_score
FROM raw_events
WHERE event_type = 'quiz_completed'
AND user_id = ?
GROUP BY date(created_at)
ORDER BY date;
```

## Real-time Processing

### Stream Processor
```go
type EventProcessor struct {
    db *sql.DB
    metricsCache *cache.Cache
}

func (p *EventProcessor) ProcessEvent(ctx context.Context, event Event) error {
    // 1. Store raw event
    if err := p.storeRawEvent(ctx, event); err != nil {
        return err
    }

    // 2. Update real-time metrics
    switch event.EventType {
    case "lesson_completed":
        if err := p.updateCompletionMetrics(ctx, event); err != nil {
            return err
        }
    case "quiz_completed":
        if err := p.updatePerformanceMetrics(ctx, event); err != nil {
            return err
        }
    }

    // 3. Check for anomalies
    p.checkAnomalies(ctx, event)

    return nil
}
```

### Batch Aggregation
```go
func (p *EventProcessor) RunHourlyAggregation(ctx context.Context) error {
    hourAgo := time.Now().Add(-1 * time.Hour)

    // Aggregate enrollments
    if err := p.aggregateEnrollments(ctx, hourAgo); err != nil {
        return err
    }

    // Aggregate engagement
    if err := p.aggregateEngagement(ctx, hourAgo); err != nil {
        return err
    }

    // Aggregate performance
    if err := p.aggregatePerformance(ctx, hourAgo); err != nil {
        return err
    }

    return nil
}
```

## Caching Strategy

### Cache Layers
```
┌─────────────────────────────────────────┐
│           Client (Browser)              │
└─────────────────────────────────────────┘
                  │
┌─────────────────▼───────────────────────┐
│         CDN (Static Assets)             │
└─────────────────────────────────────────┘
                  │
┌─────────────────▼───────────────────────┐
│      Redis (Query Results)              │
│      - TTL: 5 minutes                   │
│      - Cache popular dashboards         │
└─────────────────────────────────────────┘
                  │
┌─────────────────▼───────────────────────┐
│     Application Cache (Go)              │
│      - TTL: 1 minute                    │
│      - Recent metrics                   │
└─────────────────────────────────────────┘
                  │
┌─────────────────▼───────────────────────┐
│         SQLite Database                 │
└─────────────────────────────────────────┘
```

### Cache Invalidation
```go
func (s *AnalyticsService) InvalidateCache(courseID string) {
    // Invalidate course metrics
    s.cache.Delete(fmt.Sprintf("course:%s:metrics", courseID))
    
    // Invalidate user progress
    s.cache.Delete(fmt.Sprintf("course:%s:progress:*", courseID))
}
```

## API Implementation

### Get Course Overview
```go
func (s *AnalyticsService) GetCourseOverview(ctx context.Context, courseID string) (*CourseOverview, error) {
    // Check cache first
    if cached, ok := s.cache.Get(fmt.Sprintf("course:%s:overview", courseID)); ok {
        return cached.(*CourseOverview), nil
    }

    // Query metrics
    overview := &CourseOverview{}
    
    // Total enrollments
    err := s.db.QueryRowContext(ctx, `
        SELECT COUNT(DISTINCT user_id) 
        FROM raw_events 
        WHERE course_id = ? AND event_type = 'course_enrolled'
    `, courseID).Scan(&overview.TotalEnrollments)
    
    // Completion rate
    err = s.db.QueryRowContext(ctx, `
        SELECT 
            COUNT(CASE WHEN event_type = 'lesson_completed' THEN 1 END) * 1.0 /
            COUNT(CASE WHEN event_type = 'lesson_started' THEN 1 END)
        FROM raw_events 
        WHERE course_id = ?
    `, courseID).Scan(&overview.CompletionRate)

    // Cache result
    s.cache.Set(fmt.Sprintf("course:%s:overview", courseID), overview, 5*time.Minute)

    return overview, nil
}
```

## Performance Optimization

### Query Optimization
```sql
-- Use covering indexes
CREATE INDEX idx_events_analysis ON raw_events(course_id, event_type, created_at, user_id);

-- Materialize complex queries
CREATE VIEW daily_course_metrics AS
SELECT 
    course_id,
    date(created_at) as date,
    COUNT(DISTINCT CASE WHEN event_type = 'course_enrolled' THEN user_id END) as new_enrollments,
    COUNT(DISTINCT CASE WHEN event_type = 'lesson_started' THEN user_id END) as active_users
FROM raw_events
GROUP BY course_id, date(created_at);
```

### Batch Processing
```go
func (p *EventProcessor) ProcessBatch(ctx context.Context, events []Event) error {
    tx, err := p.db.BeginTx(ctx, nil)
    if err != nil {
        return err
    }
    defer tx.Rollback()

    // Batch insert events
    stmt, _ := tx.PrepareContext(ctx, `
        INSERT INTO raw_events (event_id, user_id, course_id, event_type, event_data)
        VALUES (?, ?, ?, ?, ?)
    `)
    defer stmt.Close()

    for _, event := range events {
        _, err = stmt.ExecContext(ctx, event.ID, event.UserID, event.CourseID, 
            event.EventType, event.Data)
        if err != nil {
            return err
        }
    }

    return tx.Commit()
}
```

## Monitoring

### Key Metrics
- Event processing latency
- Queue depth
- Error rate
- Cache hit rate
- Query performance

### Alerting
```yaml
alerts:
  - name: high_event_processing_latency
    condition: avg(latency) > 100ms
    severity: warning
    
  - name: event_processing_errors
    condition: error_rate > 1%
    severity: critical
    
  - name: queue_depth_high
    condition: queue_size > 10000
    severity: warning
```

## Document Metadata
- Owner: Engineering Department
- Product: Learning Analytics
- Last Updated: 2024-01-15
- Domain: engineering
