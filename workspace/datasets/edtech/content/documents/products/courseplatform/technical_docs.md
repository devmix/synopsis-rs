# Course Platform - Technical Documentation

## Architecture Overview

The Course Platform is built using a modern microservices architecture designed for scalability and maintainability.

## System Components

### API Gateway
- **Purpose:** Entry point for all client requests
- **Technology:** Go with chi router
- **Responsibilities:**
  - Request routing
  - Authentication
  - Rate limiting
  - Request validation

### Course Service
- **Purpose:** Core course management
- **Technology:** Go, SQLite
- **Responsibilities:**
  - Course CRUD operations
  - Lesson management
  - Category handling
  - Search indexing

### User Service
- **Purpose:** User management and authentication
- **Technology:** Go, SQLite
- **Responsibilities:**
  - User registration
  - Authentication
  - Profile management
  - Role management

### Enrollment Service
- **Purpose:** Course enrollment and progress tracking
- **Technology:** Go, SQLite
- **Responsibilities:**
  - Enrollment management
  - Progress tracking
  - Completion certificates
  - Learning paths

### Payment Service
- **Purpose:** Transaction processing
- **Technology:** Go, Stripe API
- **Responsibilities:**
  - Payment processing
  - Refund handling
  - Subscription management
  - Invoice generation

## Data Flow

### Course Enrollment Flow
```
Client → API Gateway → Enrollment Service
                           ↓
                    User Service (verify user)
                           ↓
                    Course Service (verify course)
                           ↓
                    Payment Service (if paid course)
                           ↓
                    Database (create enrollment)
                           ↓
                    Event Bus (enrollment.created)
                           ↓
               Analytics Service | Email Service
```

### Video Streaming Flow
```
Client → API Gateway → Course Service
                           ↓
                    Generate signed URL
                           ↓
                    Client → CloudFront CDN
                           ↓
                    Stream video
```

## Database Design

### Entity Relationship
```
┌─────────────┐       ┌─────────────┐       ┌─────────────┐
│    Users    │       │   Courses   │       │    Lessons  │
├─────────────┤       ├─────────────┤       ├─────────────┤
│ id          │       │ id          │       │ id          │
│ email       │◄──────│ instructor_id│      │ course_id   │
│ password    │       │ title       │       │ title       │
│ role        │       │ description │       │ content     │
│ created_at  │       │ status      │       │ video_url   │
└─────────────┘       │ price       │       │ duration    │
                      └──────┬──────┘       └──────┬──────┘
                             │                     │
                             │                     │
                      ┌──────▼────────────────────▼──────┐
                      │         Enrollments              │
                      ├──────────────────────────────────┤
                      │ id                               │
                      │ user_id ────────────────────────►│
                      │ course_id ──────────────────────►│
                      │ progress_percent                 │
                      │ completed_at                     │
                      └──────────────────────────────────┘
```

## API Specifications

### Create Course
```http
POST /api/v1/courses
Authorization: Bearer {token}
Content-Type: application/json

{
    "title": "Introduction to Python",
    "description": "Learn Python from scratch",
    "category": "programming",
    "difficulty_level": "beginner",
    "price_cents": 4999,
    "lessons": [
        {
            "title": "Getting Started",
            "content": "Welcome to Python!",
            "duration_minutes": 10
        }
    ]
}
```

### Enroll in Course
```http
POST /api/v1/courses/{id}/enroll
Authorization: Bearer {token}

{
    "payment_method_id": "pm_123456"
}
```

### Get Progress
```http
GET /api/v1/enrollments/{id}/progress
Authorization: Bearer {token}

Response:
{
    "course_id": 123,
    "total_lessons": 20,
    "completed_lessons": 15,
    "progress_percent": 75,
    "estimated_time_remaining": "2 hours"
}
```

## Error Handling

### Error Response Format
```json
{
    "error": {
        "code": "COURSE_NOT_FOUND",
        "message": "The requested course does not exist",
        "details": {
            "course_id": 999
        }
    }
}
```

### Error Codes
| Code | HTTP Status | Description |
|------|-------------|-------------|
| UNAUTHORIZED | 401 | Invalid or missing authentication |
| FORBIDDEN | 403 | Insufficient permissions |
| NOT_FOUND | 404 | Resource not found |
| VALIDATION_ERROR | 400 | Invalid input data |
| PAYMENT_FAILED | 402 | Payment processing failed |
| INTERNAL_ERROR | 500 | Server error |

## Testing Strategy

### Unit Tests
```go
func TestCourseService_Create(t *testing.T) {
    tests := []struct {
        name    string
        input   CreateCourseInput
        wantErr bool
    }{
        {
            name: "valid course",
            input: CreateCourseInput{
                Title: "Test Course",
                Description: "Test Description",
            },
            wantErr: false,
        },
        {
            name: "missing title",
            input: CreateCourseInput{},
            wantErr: true,
        },
    }

    for _, tt := range tests {
        t.Run(tt.name, func(t *testing.T) {
            svc := NewCourseService(db)
            _, err := svc.Create(context.Background(), tt.input)
            if (err != nil) != tt.wantErr {
                t.Errorf("Create() error = %v, wantErr %v", err, tt.wantErr)
            }
        })
    }
}
```

### Integration Tests
```go
func TestEnrollmentFlow(t *testing.T) {
    // Setup test database
    db := setupTestDB(t)
    
    // Create test user
    userID := createTestUser(db)
    
    // Create test course
    courseID := createTestCourse(db)
    
    // Enroll user
    enrollmentID := enrollUser(t, db, userID, courseID)
    
    // Verify enrollment
    enrollment := getEnrollment(t, db, enrollmentID)
    assert.Equal(t, courseID, enrollment.CourseID)
    assert.Equal(t, userID, enrollment.UserID)
}
```

## Deployment

### Environment Variables
```bash
# Database
DATABASE_URL=sqlite:///data/courses.db

# Authentication
JWT_SECRET=your-secret-key
JWT_EXPIRY=24h

# Payment
STRIPE_SECRET_KEY=sk_test_...
STRIPE_WEBHOOK_SECRET=whsec_...

# Server
SERVER_PORT=8080
LOG_LEVEL=info
```

### Docker Configuration
```dockerfile
FROM golang:1.21-alpine AS builder
WORKDIR /app
COPY . .
RUN go build -o main ./cmd/api

FROM alpine:latest
RUN apk --no-cache add ca-certificates
WORKDIR /root/
COPY --from=builder /app/main .
EXPOSE 8080
CMD ["./main"]
```

### Kubernetes Deployment
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: course-platform
spec:
  replicas: 3
  selector:
    matchLabels:
      app: course-platform
  template:
    metadata:
      labels:
        app: course-platform
    spec:
      containers:
      - name: api
        image: learn-tech/course-platform:latest
        ports:
        - containerPort: 8080
        env:
        - name: DATABASE_URL
          valueFrom:
            secretKeyRef:
              name: db-secret
              key: url
```

## Monitoring

### Metrics
- Request rate (requests/second)
- Error rate (errors/requests)
- Latency (p50, p95, p99)
- Database query time
- Cache hit rate

### Logging
- Structured JSON logs
- Request ID correlation
- Log levels (debug, info, warn, error)
- Centralized log aggregation

## Security Checklist

- [ ] HTTPS enforced
- [ ] Input validation on all endpoints
- [ ] SQL injection prevention (parameterized queries)
- [ ] XSS prevention (output encoding)
- [ ] CSRF protection
- [ ] Rate limiting enabled
- [ ] Authentication required for protected routes
- [ ] Authorization checks on resources
- [ ] Sensitive data encrypted
- [ ] Security headers configured
- [ ] Dependencies scanned for vulnerabilities

## Document Metadata
- Owner: Engineering Department
- Product: Course Platform
- Last Updated: 2024-01-15
- Domain: engineering
