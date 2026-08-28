# Adaptive Learning - Technical Documentation

## Architecture Overview

The Adaptive Learning system combines multiple machine learning models with real-time decision engines to deliver personalized learning experiences.

## System Components

### 1. Profiling Service

**Purpose:** Create and maintain student profiles

**Responsibilities:**
- Initial assessment processing
- Continuous profile updates
- Learning style detection
- Pace calculation

**Technology:** Go, SQLite, ML inference

### 2. Knowledge Tracing Engine

**Purpose:** Track student mastery of concepts

**Responsibilities:**
- Bayesian knowledge tracing
- Deep knowledge tracing (LSTM)
- Mastery probability calculation
- Confidence estimation

**Technology:** Python (TensorFlow), gRPC

### 3. Recommendation Engine

**Purpose:** Suggest optimal next content

**Responsibilities:**
- Content ranking
- Collaborative filtering
- Content-based filtering
- Diversity optimization

**Technology:** Python (scikit-learn), Redis

### 4. Difficulty Adapter

**Purpose:** Adjust content difficulty dynamically

**Responsibilities:**
- Item Response Theory modeling
- Difficulty prediction
- Hint generation
- Challenge problem selection

**Technology:** Go, SQLite

### 5. Decision Router

**Purpose:** Orchestrate adaptive decisions

**Responsibilities:**
- Model selection
- Decision fusion
- Fallback handling
- A/B test routing

**Technology:** Go, Redis

## System Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Client (Course Player)                    │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                   Adaptive API Gateway                       │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  Authentication │ Rate Limiting │ Request Routing     │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐    ┌───────────────┐    ┌───────────────┐
│   Profiling   │    │ Knowledge     │    │ Recommendation│
│   Service     │    │ Tracing       │    │ Engine        │
├───────────────┤    ├───────────────┤    ├───────────────┤
│ - Assessment  │    │ - BKT Model   │    │ - Collaborative│
│ - Learning    │    │ - DKT Model   │    │ - Content-    │
│   Style       │    │ - Mastery     │    │   Based       │
│ - Pace        │    │   Tracking    │    │ - Ranking     │
└───────────────┘    └───────────────┘    └───────────────┘
        │                     │                     │
        └─────────────────────┼─────────────────────┘
                              ▼
                    ┌───────────────────┐
                    │ Decision Router   │
                    ├───────────────────┤
                    │ - Fusion Logic    │
                    │ - Fallback        │
                    │ - A/B Testing     │
                    └───────────────────┘
                              │
                              ▼
                    ┌───────────────────┐
                    │   Response        │
                    │   Formatter       │
                    └───────────────────┘
```

## Data Models

### Student Profile
```go
type StudentProfile struct {
    ID              string
    UserID          string
    CreatedAt       time.Time
    UpdatedAt       time.Time
    
    // Knowledge Map
    KnowledgeAreas  map[string]*KnowledgeLevel
    
    // Learning Preferences
    LearningStyle   *LearningStyle
    PreferredPace   float64     // lessons per week
    OptimalTime     string      // time of day
    SessionLength   int         // minutes
    
    // Performance History
    AvgScore        float64
    CompletionRate  float64
    EngagementScore float64
    
    // Metadata
    Metadata        map[string]interface{}
}

type KnowledgeLevel struct {
    Concept      string
    Mastery      float64     // 0.0 - 1.0
    Confidence   float64     // 0.0 - 1.0
    LastPracticed time.Time
    TimesPracticed int
    ErrorPattern  []string
}

type LearningStyle struct {
    Visual       float64
    Auditory     float64
    Kinesthetic  float64
}
```

### Content Metadata
```go
type ContentMetadata struct {
    ID          string
    Type        string      // lesson, quiz, problem, video
    Concepts    []string    // associated concepts
    Difficulty  float64     // 1.0 - 5.0
    Prerequisites []string  // prerequisite content IDs
    EstimatedTime int       // minutes
    Tags        []string
    Format      string      // text, video, interactive
}
```

### Adaptation Decision
```go
type AdaptationDecision struct {
    UserID        string
    CurrentContent string
    RecommendedContent string
    Reason        string
    Confidence    float64
    Alternatives  []string
    Adjustments   *ContentAdjustments
}

type ContentAdjustments struct {
    DifficultyAdjustment float64
    HintLevel           int
    SkipContent         bool
    AddRemedial         bool
    TimeExtension       int // minutes
}
```

## Machine Learning Models

### 1. Bayesian Knowledge Tracing (BKT)

**Implementation:**
```python
class BayesianKnowledgeTracing:
    def __init__(self, concepts):
        self.concepts = concepts
        # P(L0): Initial mastery probability
        self.p_initial = {c: 0.1 for c in concepts}
        # P(T): Transition probability (learning rate)
        self.p_transition = {c: 0.15 for c in concepts}
        # P(S): Slip probability (correct answer despite no mastery)
        self.p_slip = {c: 0.1 for c in concepts}
        # P(G): Guess probability (correct answer by chance)
        self.p_guess = {c: 0.3 for c in concepts}
    
    def update(self, concept: str, correct: bool) -> float:
        """Update mastery probability based on response."""
        p_l = self.p_initial[concept]
        p_t = self.p_transition[concept]
        p_s = self.p_slip[concept]
        p_g = self.p_guess[concept]
        
        if correct:
            # P(L|correct) = P(L) * (1-P(S)) / P(correct)
            p_correct = p_l * (1 - p_s) + (1 - p_l) * p_g
            p_l_new = (p_l * (1 - p_s)) / p_correct
        else:
            # P(L|incorrect) = P(L) * P(S) / P(incorrect)
            p_incorrect = p_l * p_s + (1 - p_l) * (1 - p_g)
            p_l_new = (p_l * p_s) / p_incorrect
        
        # Apply transition (learning can happen)
        p_l_final = p_l_new + (1 - p_l_new) * p_t
        
        self.p_initial[concept] = p_l_final
        return p_l_final
```

### 2. Deep Knowledge Tracing (DKT)

**Model Architecture:**
```python
class DeepKnowledgeTracing(nn.Module):
    def __init__(self, num_concepts, embed_size=100, hidden_size=200):
        super().__init__()
        self.embedding = nn.Embedding(num_concepts * 2, embed_size)
        self.lstm = nn.LSTM(embed_size, hidden_size, batch_first=True)
        self.fc = nn.Linear(hidden_size, num_concepts)
        self.sigmoid = nn.Sigmoid()
    
    def forward(self, sequence):
        # sequence: [batch, seq_len] with concept IDs
        # even = question, odd = response
        embedded = self.embedding(sequence)
        lstm_out, _ = self.lstm(embedded)
        output = self.fc(lstm_out[:, -1, :])
        return self.sigmoid(output)
```

### 3. Item Response Theory (IRT)

**Implementation:**
```python
class ItemResponseTheory:
    """3-parameter logistic model."""
    
    def __init__(self):
        # Item parameters: difficulty (b), discrimination (a), guessing (c)
        self.item_params = {}
    
    def register_item(self, item_id, difficulty, discrimination=1.0, guessing=0.2):
        self.item_params[item_id] = {
            'b': difficulty,
            'a': discrimination,
            'c': guessing
        }
    
    def probability_correct(self, ability, item_id):
        """Calculate P(correct) given student ability."""
        params = self.item_params[item_id]
        a, b, c = params['a'], params['b'], params['c']
        
        # 3PL model: P = c + (1-c) * sigmoid(a * (theta - b))
        z = a * (ability - b)
        p = c + (1 - c) * (1 / (1 + np.exp(-z)))
        return p
    
    def estimate_ability(self, item_responses):
        """Estimate student ability from responses."""
        # Maximum likelihood estimation
        ability = 0.0  # Initial guess
        
        for item_id, correct in item_responses:
            # Iterative update (simplified)
            p = self.probability_correct(ability, item_id)
            error = correct - p
            ability += 0.1 * error  # Learning rate
        
        return np.clip(ability, -3, 3)
```

### 4. Recommendation Engine

**Collaborative Filtering:**
```python
class CollaborativeRecommender:
    def __init__(self):
        self.user_item_matrix = None
        self.item_similarities = None
    
    def fit(self, user_interactions):
        """Build user-item interaction matrix."""
        # user_interactions: [(user_id, item_id, score), ...]
        users = list(set(u for u, _, _ in user_interactions))
        items = list(set(i for _, i, _ in user_interactions))
        
        self.user_item_matrix = np.zeros((len(users), len(items)))
        self.user_map = {u: i for i, u in enumerate(users)}
        self.item_map = {it: i for i, it in enumerate(items)}
        
        for user, item, score in user_interactions:
            u_idx = self.user_map[user]
            i_idx = self.item_map[item]
            self.user_item_matrix[u_idx, i_idx] = score
        
        # Compute item-item similarities
        self._compute_item_similarities()
    
    def recommend(self, user_id, n=5):
        """Generate recommendations for user."""
        if user_id not in self.user_map:
            return []  # Cold start
        
        user_idx = self.user_map[user_id]
        user_ratings = self.user_item_matrix[user_idx]
        
        # Predict ratings for unrated items
        predictions = []
        for item_idx, rating in enumerate(user_ratings):
            if rating == 0:  # Not rated
                predicted = self._predict_rating(user_idx, item_idx)
                predictions.append((item_idx, predicted))
        
        # Return top N
        predictions.sort(key=lambda x: x[1], reverse=True)
        return [self._item_id_from_idx(idx) for idx, _ in predictions[:n]]
    
    def _predict_rating(self, user_idx, item_idx):
        """Predict rating using item-based collaborative filtering."""
        item_ratings = self.user_item_matrix[:, item_idx]
        user_ratings = self.user_item_matrix[user_idx]
        
        # Weighted average of similar items
        numerator = 0
        denominator = 0
        
        for other_item_idx, sim in enumerate(self.item_similarities[item_idx]):
            if user_ratings[other_item_idx] > 0:
                numerator += sim * user_ratings[other_item_idx]
                denominator += abs(sim)
        
        return numerator / denominator if denominator > 0 else 3.0
```

## API Design

### Get Recommendations
```http
POST /api/v1/adaptive/recommendations
Authorization: Bearer {token}
Content-Type: application/json

{
    "user_id": "user_123",
    "current_context": {
        "course_id": "course_456",
        "current_lesson": "lesson_789"
    }
}

Response:
{
    "recommendations": [
        {
            "content_id": "lesson_abc",
            "type": "lesson",
            "reason": "Builds on your strong Python skills",
            "confidence": 0.87,
            "estimated_time": 25,
            "prerequisites_met": true
        },
        {
            "content_id": "quiz_def",
            "type": "quiz",
            "reason": "Practice recommended for Data Structures",
            "confidence": 0.92,
            "estimated_time": 15,
            "difficulty_adjusted": true
        }
    ],
    "learning_path_update": {
        "next_milestone": "lesson_xyz",
        "estimated_completion": "2024-02-15"
    }
}
```

### Update Progress
```http
POST /api/v1/adaptive/progress
Authorization: Bearer {token}
Content-Type: application/json

{
    "user_id": "user_123",
    "content_id": "lesson_789",
    "completion_status": "completed",
    "score": 0.85,
    "time_spent_seconds": 1200,
    "hints_used": 2,
    "attempts": 1
}

Response:
{
    "profile_updated": true,
    "mastery_changes": [
        {
            "concept": "Python Basics",
            "previous_mastery": 0.72,
            "new_mastery": 0.80
        }
    ],
    "next_recommendation": "lesson_abc"
}
```

## Performance Optimization

### Caching Strategy
```go
type AdaptiveCache struct {
    profiles    *cache.Cache  // TTL: 1 hour
    recommendations *cache.Cache  // TTL: 5 minutes
    models      *cache.Cache  // TTL: 24 hours
}

func (c *AdaptiveCache) GetProfile(userID string) (*StudentProfile, bool) {
    if cached, ok := c.profiles.Get(userID); ok {
        return cached.(*StudentProfile), true
    }
    return nil, false
}

func (c *AdaptiveCache) SetProfile(userID string, profile *StudentProfile) {
    c.profiles.Set(userID, profile, cache.DefaultExpiration)
}
```

### Model Serving
```go
type ModelServer struct {
    clients map[string]*grpc.ClientConn
}

func (s *ModelServer) GetMastery(userID, concept string) (float64, error) {
    // Connect to knowledge tracing service
    client := s.getOrCreateClient("knowledge-tracing")
    
    resp, err := client.GetMastery(context.Background(), &MasteryRequest{
        UserId:  userID,
        Concept: concept,
    })
    
    if err != nil {
        // Fallback to cached value
        return s.getFallbackMastery(userID, concept)
    }
    
    return resp.Mastery, nil
}
```

## Error Handling

### Fallback Strategies
```go
func (s *AdaptiveService) GetRecommendation(ctx context.Context, userID string) ([]Recommendation, error) {
    // Try primary ML model
    recs, err := s.mlRecommender.Recommend(ctx, userID)
    if err == nil && len(recs) > 0 {
        return recs, nil
    }
    
    // Fallback 1: Rule-based recommender
    recs, err = s.ruleBasedRecommender.Recommend(ctx, userID)
    if err == nil && len(recs) > 0 {
        return recs, nil
    }
    
    // Fallback 2: Popular content
    recs = s.popularContentRecommender.Recommend(userID)
    if len(recs) > 0 {
        return recs, nil
    }
    
    // Ultimate fallback: Default course path
    return s.getDefaultPath(userID), nil
}
```

## Monitoring

### Key Metrics
- Model inference latency
- Recommendation acceptance rate
- Profile update frequency
- Fallback trigger rate

### Alerting
```yaml
alerts:
  - name: high_inference_latency
    condition: p95_latency > 200ms
    severity: warning
    
  - name: low_recommendation_acceptance
    condition: acceptance_rate < 30%
    severity: warning
    
  - name: model_error_rate
    condition: error_rate > 1%
    severity: critical
```

## Document Metadata
- Owner: Engineering Department
- Product: Adaptive Learning
- Last Updated: 2024-01-15
- Domain: engineering
