# API Design Guidelines

## Overview
These guidelines ensure consistent, intuitive, and maintainable API design across all LearnTech Solutions products.

## REST API Principles

### Resource Naming
```
# Use nouns, not verbs
GET    /api/v1/courses           # List courses
GET    /api/v1/courses/{id}      # Get course
POST   /api/v1/courses           # Create course
PUT    /api/v1/courses/{id}      # Update course
DELETE /api/v1/courses/{id}      # Delete course

# Use nested resources for relationships
GET    /api/v1/courses/{id}/lessons
POST   /api/v1/courses/{id}/lessons
```

### HTTP Methods
| Method | Usage | Idempotent |
|--------|-------|------------|
| GET | Retrieve resources | Yes |
| POST | Create resources | No |
| PUT | Full update | Yes |
| PATCH | Partial update | No |
| DELETE | Remove resources | Yes |

### Status Codes
```go
// Success
200 OK - Successful GET, PUT, PATCH
201 Created - Successful POST
204 No Content - Successful DELETE

// Client Errors
400 Bad Request - Invalid input
401 Unauthorized - Authentication required
403 Forbidden - Insufficient permissions
404 Not Found - Resource doesn't exist
409 Conflict - Resource conflict

// Server Errors
500 Internal Server Error
502 Bad Gateway
503 Service Unavailable
```

## Request/Response Format

### Request Headers
```
Content-Type: application/json
Authorization: Bearer {token}
Accept: application/json
X-Request-ID: {uuid}
```

### Response Format
```json
{
    "success": true,
    "data": {
        "id": "123",
        "name": "Introduction to Python",
        "description": "Learn Python basics"
    },
    "meta": {
        "request_id": "abc-123",
        "timestamp": "2024-01-15T10:30:00Z"
    }
}
```

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
    },
    "meta": {
        "request_id": "abc-123",
        "timestamp": "2024-01-15T10:30:00Z"
    }
}
```

## Pagination

### Query Parameters
```
GET /api/v1/courses?page=1&limit=20&sort=name&order=asc
```

### Pagination Response
```json
{
    "success": true,
    "data": [...],
    "pagination": {
        "page": 1,
        "limit": 20,
        "total": 150,
        "total_pages": 8,
        "has_next": true,
        "has_prev": false
    }
}
```

## Filtering and Sorting

### Filtering
```
GET /api/v1/courses?status=active&instructor=john&category=programming
```

### Sorting
```
GET /api/v1/courses?sort=name,-created_at  # name asc, created_at desc
```

## Versioning

### URL Versioning
```
/api/v1/courses
/api/v2/courses
```

### Deprecation Policy
- Minimum 6 months notice for deprecated endpoints
- Version headers indicate deprecation
- Migration guides provided

## Authentication

### OAuth 2.0 Flow
```go
// Token endpoint
POST /api/v1/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=client_credentials
&client_id={client_id}
&client_secret={client_secret}

// Response
{
    "access_token": "eyJhbGciOi...",
    "token_type": "Bearer",
    "expires_in": 3600,
    "scope": "read write"
}
```

### Scopes
| Scope | Description |
|-------|-------------|
| read | Read-only access |
| write | Create and update resources |
| admin | Full administrative access |

## Rate Limiting

### Headers
```
X-RateLimit-Limit: 1000
X-RateLimit-Remaining: 999
X-RateLimit-Reset: 1705312200
```

### Limits by Tier
| Tier | Requests/hour |
|------|---------------|
| Free | 100 |
| Pro | 1000 |
| Enterprise | 10000 |

## API Documentation

### OpenAPI Specification
All APIs must have OpenAPI 3.0 documentation:
```yaml
openapi: 3.0.0
info:
  title: Course API
  version: 1.0.0
paths:
  /courses:
    get:
      summary: List courses
      responses:
        '200':
          description: Successful response
```

### Documentation Requirements
- Endpoint description
- Request parameters
- Response schema
- Error codes
- Example requests/responses

## Best Practices

### Performance
- Use gzip compression
- Implement caching (ETag, Last-Modified)
- Support field filtering: `?fields=id,name`
- Use bulk endpoints for batch operations

### Security
- Always use HTTPS
- Validate and sanitize all inputs
- Implement CORS properly
- Use rate limiting
- Log security events

### Observability
- Include request IDs in responses
- Log API access and errors
- Track API metrics (latency, error rate)
- Implement health check endpoints

## Document Metadata
- Owner: Engineering Department
- Last Updated: 2024-01-15
- Domain: engineering
