#[derive(Debug)]
pub enum NodeError {
    NetworkIO(String),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::NetworkIO(e) => write!(f, "{}", e),
        }
    }
}
