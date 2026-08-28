# Database Schema Design

## Overview
LearnTech Solutions uses SQLite for its primary data store. This document outlines database schema design principles and conventions.

## Design Principles

### Normalization
- Aim for 3rd Normal Form (3NF)
- Denormalize only for performance with documented justification
- Maintain referential integrity with foreign keys

### Naming Conventions
```sql
-- Tables: snake_case, plural
CREATE TABLE courses (
    -- ...
);

-- Columns: snake_case
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    first_name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Indexes: idx_table_column
CREATE INDEX idx_courses_created_at ON courses(created_at);

-- Foreign keys: table_id
CREATE TABLE enrollments (
    user_id INTEGER REFERENCES users(id),
    course_id INTEGER REFERENCES courses(id)
);
```

## Core Tables

### Users Table
```sql
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    first_name TEXT NOT NULL,
    last_name TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('student', 'instructor', 'admin')),
    is_active INTEGER NOT NULL DEFAULT 1,
    last_login TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_users_email ON users(email);
CREATE INDEX idx_users_role ON users(role);
```

### Courses Table
```sql
CREATE TABLE courses (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    instructor_id INTEGER NOT NULL REFERENCES users(id),
    title TEXT NOT NULL,
    description TEXT,
    slug TEXT UNIQUE NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('draft', 'published', 'archived')),
    category TEXT NOT NULL,
    difficulty_level TEXT CHECK(difficulty_level IN ('beginner', 'intermediate', 'advanced')),
    duration_minutes INTEGER,
    price_cents INTEGER NOT NULL DEFAULT 0,
    thumbnail_url TEXT,
    published_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_courses_instructor ON courses(instructor_id);
CREATE INDEX idx_courses_status ON courses(status);
CREATE INDEX idx_courses_category ON courses(category);
CREATE INDEX idx_courses_slug ON courses(slug);
```

### Lessons Table
```sql
CREATE TABLE lessons (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    course_id INTEGER NOT NULL REFERENCES courses(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    content TEXT,
    video_url TEXT,
    duration_minutes INTEGER,
    sequence_num INTEGER NOT NULL,
    is_free_preview INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(course_id, sequence_num)
);

CREATE INDEX idx_lessons_course ON lessons(course_id);
CREATE INDEX idx_lessons_sequence ON lessons(course_id, sequence_num);
```

### Enrollments Table
```sql
CREATE TABLE enrollments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    course_id INTEGER NOT NULL REFERENCES courses(id) ON DELETE CASCADE,
    enrolled_at TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at TEXT,
    progress_percent INTEGER NOT NULL DEFAULT 0,
    UNIQUE(user_id, course_id)
);

CREATE INDEX idx_enrollments_user ON enrollments(user_id);
CREATE INDEX idx_enrollments_course ON enrollments(course_id);
CREATE INDEX idx_enrollments_progress ON enrollments(progress_percent);
```

### Progress Tracking
```sql
CREATE TABLE lesson_progress (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    enrollment_id INTEGER NOT NULL REFERENCES enrollments(id) ON DELETE CASCADE,
    lesson_id INTEGER NOT NULL REFERENCES lessons(id) ON DELETE CASCADE,
    is_completed INTEGER NOT NULL DEFAULT 0,
    completed_at TEXT,
    time_spent_seconds INTEGER NOT NULL DEFAULT 0,
    last_accessed_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(enrollment_id, lesson_id)
);

CREATE INDEX idx_progress_enrollment ON lesson_progress(enrollment_id);
CREATE INDEX idx_progress_lesson ON lesson_progress(lesson_id);
CREATE INDEX idx_progress_completed ON lesson_progress(is_completed);
```

## Facts Table (GraphRAG)
```sql
CREATE TABLE facts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    domain TEXT NOT NULL DEFAULT 'default',
    statement TEXT NOT NULL,
    subject_entity_id INTEGER,
    predicate TEXT NOT NULL,
    object_entity_id INTEGER,
    attributes TEXT,
    confidence REAL NOT NULL DEFAULT 1.0,
    status TEXT NOT NULL CHECK(status IN ('pending', 'approved', 'rejected', 'review')),
    valid_from TEXT,
    valid_to TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_facts_domain ON facts(domain);
CREATE INDEX idx_facts_status ON facts(status);
CREATE INDEX idx_facts_confidence ON facts(confidence);
CREATE INDEX idx_facts_predicate ON facts(predicate);

-- FTS5 for full-text search
CREATE VIRTUAL TABLE facts_fts USING fts5(
    statement,
    content='facts',
    content_rowid='id'
);
```

### Fact Sources Table
```sql
CREATE TABLE fact_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_id INTEGER NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
    document_id INTEGER REFERENCES documents(id) ON DELETE SET NULL,
    chunk_id INTEGER,
    page INTEGER,
    quote TEXT,
    extracted_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_fact_sources_fact ON fact_sources(fact_id);
CREATE INDEX idx_fact_sources_chunk ON fact_sources(chunk_id);
```

## Documents and Chunks (RAG)
```sql
CREATE TABLE documents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_type TEXT NOT NULL,
    original_path TEXT NOT NULL,
    metadata_json TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_documents_source_type ON documents(source_type);
CREATE INDEX idx_documents_path ON documents(original_path);

CREATE TABLE chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    chunk_text TEXT NOT NULL,
    sequence_num INTEGER NOT NULL,
    start_offset INTEGER,
    end_offset INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(doc_id, sequence_num)
);

CREATE INDEX idx_chunks_doc ON chunks(doc_id);

-- FTS5 for chunk search
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    chunk_text,
    content='chunks',
    content_rowid='id'
);

-- Vector storage (using JSON format)
CREATE TABLE chunks_vec (
    chunk_id INTEGER PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
    vector TEXT NOT NULL
);
```

## Entity Resolution Tables
```sql
CREATE TABLE entities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    description TEXT,
    canonical_name TEXT,
    merged_into INTEGER REFERENCES entities(id),
    confidence REAL NOT NULL DEFAULT 1.0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_entities_name ON entities(name);
CREATE INDEX idx_entities_type ON entities(type);
CREATE INDEX idx_entities_canonical ON entities(canonical_name);
CREATE INDEX idx_entities_merged ON entities(merged_into);
```

## Indexing Strategies

### When to Add Indexes
- Columns used in WHERE clauses
- Columns used in JOIN conditions
- Columns used in ORDER BY
- Columns with high selectivity

### Index Maintenance
```sql
-- Analyze tables for query optimizer
ANALYZE;

-- Rebuild indexes if fragmented
REINDEX;

-- Vacuum to reclaim space
VACUUM;
```

## Migration Guidelines

### Adding Columns
```sql
-- Add column with default (non-blocking)
ALTER TABLE users ADD COLUMN phone TEXT;

-- Add column with NOT NULL requires default
ALTER TABLE courses ADD COLUMN is_featured INTEGER NOT NULL DEFAULT 0;
```

### Creating Indexes Concurrently
```sql
-- Create index (SQLite does this automatically non-blocking for reads)
CREATE INDEX idx_users_created_at ON users(created_at);
```

### Data Migrations
```sql
-- Use transactions for data migrations
BEGIN TRANSACTION;

UPDATE courses SET status = 'published' WHERE status IS NULL;

COMMIT;
```

## Performance Tips

### Query Optimization
```sql
-- Use EXPLAIN QUERY PLAN to analyze queries
EXPLAIN QUERY PLAN SELECT * FROM courses WHERE status = 'published';

-- Avoid SELECT * in production
SELECT id, title, slug FROM courses WHERE status = 'published';

-- Use LIMIT for large result sets
SELECT * FROM users ORDER BY created_at DESC LIMIT 100;
```

### Connection Pooling
```go
// Configure connection pool
db.SetMaxOpenConns(25)
db.SetMaxIdleConns(25)
db.SetConnMaxLifetime(5 * time.Minute)
```

## Document Metadata
- Owner: Engineering Department
- Last Updated: 2024-01-15
- Domain: engineering
