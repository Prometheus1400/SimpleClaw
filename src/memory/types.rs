#[derive(Debug, Clone)]
pub enum MemorizeResult {
    Inserted,
    Updated { superseded_content: String },
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredRole {
    User,
    Assistant,
    System,
    Tool,
}

impl StoredRole {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }

    pub fn from_db_str(raw: &str) -> Option<Self> {
        match raw {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "system" => Some(Self::System),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub role: StoredRole,
    pub content: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LongTermFactSummary {
    pub id: i64,
    pub content: String,
    pub kind: String,
    pub importance: i64,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct LongTermForgetMatch {
    pub id: i64,
    pub content: String,
    pub kind: String,
    pub importance: i64,
    pub similarity: f32,
}

#[derive(Debug, Clone)]
pub struct LongTermForgetResult {
    pub matches: Vec<LongTermForgetMatch>,
    pub deleted_count: usize,
    pub similarity_threshold: f32,
    pub max_matches: usize,
    pub kind_filter: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryHitStore {
    LongTerm,
}

#[derive(Debug, Clone)]
pub struct MemoryPreinjectHit {
    pub store: MemoryHitStore,
    pub content: String,
    pub kind: Option<String>,
    #[allow(dead_code)]
    pub importance: Option<i64>,
    pub raw_similarity: f32,
    pub final_score: f32,
}
