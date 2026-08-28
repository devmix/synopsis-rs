# API Documentation

**URL:** https://api.leartech-solutions.com

---

## LearnTech Solutions API

**Version:** v1  
**Base URL:** https://api.leartech-solutions.com/v1

The LearnTech Solutions API allows you to integrate our platform with your applications.

---

## Authentication

All API requests require authentication using OAuth 2.0.

### Obtaining an Access Token

```bash
curl -X POST https://api.leartech-solutions.com/v1/oauth/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d "grant_type=client_credentials" \
  -d "client_id=YOUR_CLIENT_ID" \
  -d "client_secret=YOUR_CLIENT_SECRET"
```

**Response:**
```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "scope": "read write"
}
```

### Using the Token

Include the access token in your API requests:

```bash
curl https://api.leartech-solutions.com/v1/courses \
  -H "Authorization: Bearer YOUR_ACCESS_TOKEN"
```

---

## Rate Limits

| Tier | Requests/Hour |
|------|---------------|
| Free | 100 |
| Pro | 1,000 |
| Enterprise | 10,000 |

Rate limit headers are included in every response:
```
X-RateLimit-Limit: 1000
X-RateLimit-Remaining: 999
X-RateLimit-Reset: 1705312200
```

---

## Courses API

### List Courses

```
GET /courses
```

**Query Parameters:**
| Parameter | Type | Description |
|-----------|------|-------------|
| category | string | Filter by category |
| difficulty | string | Filter by difficulty (beginner, intermediate, advanced) |
| page | integer | Page number (default: 1) |
| limit | integer | Results per page (default: 20, max: 100) |

**Response:**
```json
{
  "success": true,
  "data": [
    {
      "id": "course_123",
      "title": "Python for Data Science",
      "instructor_id": "user_456",
      "category": "programming",
      "difficulty": "beginner",
      "price_cents": 8900,
      "duration_minutes": 720,
      "lesson_count": 42,
      "rating": 4.8,
      "enrollment_count": 2341
    }
  ],
  "pagination": {
    "page": 1,
    "limit": 20,
    "total": 523,
    "total_pages": 27
  }
}
```

### Get Course Details

```
GET /courses/{course_id}
```

**Response:**
```json
{
  "success": true,
  "data": {
    "id": "course_123",
    "title": "Python for Data Science",
    "description": "Learn Python programming fundamentals...",
    "instructor": {
      "id": "user_456",
      "name": "Dr. Sarah Johnson",
      "rating": 4.9
    },
    "category": "programming",
    "difficulty": "beginner",
    "price_cents": 8900,
    "duration_minutes": 720,
    "lessons": [
      {
        "id": "lesson_1",
        "title": "Introduction to Python",
        "duration_minutes": 15,
        "is_free_preview": true
      }
    ]
  }
}
```

### Create Course

```
POST /courses
```

**Request Body:**
```json
{
  "title": "Introduction to Machine Learning",
  "description": "Learn ML fundamentals...",
  "category": "data-science",
  "difficulty": "intermediate",
  "price_cents": 9900,
  "lessons": [
    {
      "title": "What is Machine Learning?",
      "content": "...",
      "duration_minutes": 20
    }
  ]
}
```

---

## Users API

### Get User Profile

```
GET /users/{user_id}
```

**Response:**
```json
{
  "success": true,
  "data": {
    "id": "user_123",
    "name": "John Doe",
    "email": "john@example.com",
    "role": "student",
    "enrolled_courses": 5,
    "completed_courses": 3,
    "certificates": 2
  }
}
```

### Enroll in Course

```
POST /enrollments
```

**Request Body:**
```json
{
  "user_id": "user_123",
  "course_id": "course_456",
  "payment_method_id": "pm_123456"
}
```

**Response:**
```json
{
  "success": true,
  "data": {
    "enrollment_id": "enroll_789",
    "user_id": "user_123",
    "course_id": "course_456",
    "enrolled_at": "2024-01-15T10:30:00Z",
    "progress_percent": 0
  }
}
```

---

## Analytics API

### Get Course Analytics

```
GET /analytics/courses/{course_id}
```

**Response:**
```json
{
  "success": true,
  "data": {
    "course_id": "course_123",
    "overview": {
      "total_enrollments": 1234,
      "completion_rate": 0.68,
      "avg_score": 0.82,
      "avg_time_hours": 12
    },
    "engagement": {
      "daily_active_users": 450,
      "avg_session_minutes": 25,
      "lesson_completion_rate": 0.75
    },
    "performance": {
      "avg_quiz_score": 0.78,
      "assignment_submission_rate": 0.85,
      "at_risk_students": 45
    }
  }
}
```

### Get Student Progress

```
GET /analytics/students/{user_id}/progress
```

**Response:**
```json
{
  "success": true,
  "data": {
    "user_id": "user_123",
    "enrollments": [
      {
        "course_id": "course_456",
        "progress_percent": 75,
        "completed_lessons": 15,
        "total_lessons": 20,
        "last_accessed": "2024-01-15T09:00:00Z"
      }
    ]
  }
}
```

---

## Adaptive Learning API

### Get Recommendations

```
POST /adaptive/recommendations
```

**Request Body:**
```json
{
  "user_id": "user_123",
  "current_context": {
    "course_id": "course_456",
    "current_lesson": "lesson_789"
  }
}
```

**Response:**
```json
{
  "success": true,
  "data": {
    "recommendations": [
      {
        "content_id": "lesson_abc",
        "type": "lesson",
        "reason": "Builds on your strong Python skills",
        "confidence": 0.87,
        "estimated_time": 25
      }
    ],
    "learning_path_update": {
      "next_milestone": "lesson_xyz",
      "estimated_completion": "2024-02-15"
    }
  }
}
```

### Update Progress

```
POST /adaptive/progress
```

**Request Body:**
```json
{
  "user_id": "user_123",
  "content_id": "lesson_789",
  "completion_status": "completed",
  "score": 0.85,
  "time_spent_seconds": 1200,
  "hints_used": 2
}
```

---

## Error Handling

The API returns standard HTTP status codes and error responses.

### Error Response Format

```json
{
  "success": false,
  "error": {
    "code": "VALIDATION_ERROR",
    "message": "Invalid input data",
    "details": [
      {
        "field": "email",
        "message": "Invalid email format"
      }
    ]
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
| RATE_LIMITED | 429 | Too many requests |
| INTERNAL_ERROR | 500 | Server error |

---

## SDKs and Libraries

### JavaScript/TypeScript

```bash
npm install @leartech/api
```

```javascript
const LearnTech = require('@leartech/api');

const api = new LearnTech({
  apiKey: 'your_api_key'
});

const courses = await api.courses.list({ category: 'programming' });
```

### Python

```bash
pip install leartech-api
```

```python
from leartech import LearnTech

api = LearnTech(api_key='your_api_key')
courses = api.courses.list(category='programming')
```

---

## Webhooks

Subscribe to events in your dashboard or configure webhook URLs.

### Available Events

| Event | Description |
|-------|-------------|
| enrollment.created | New course enrollment |
| course.completed | Student completed a course |
| payment.completed | Payment successful |
| review.created | New course review |

### Webhook Payload

```json
{
  "event": "enrollment.created",
  "timestamp": "2024-01-15T10:30:00Z",
  "data": {
    "enrollment_id": "enroll_789",
    "user_id": "user_123",
    "course_id": "course_456"
  }
}
```

---

## Support

- **API Documentation:** https://developers.leartech-solutions.com
- **API Support:** api-support@leartech-solutions.com
- **Status Page:** https://status.leartech-solutions.com

---

*© 2024 LearnTech Solutions. All rights reserved.*
