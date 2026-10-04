//! A tiny HTTP router (fixture).
use std::collections::HashMap;

pub const MAX_ROUTES: usize = 64;

/// A route table.
pub struct Router {
    routes: HashMap<String, Box<dyn Handler>>,
}

pub enum Method {
    Get,
    Post,
}

pub trait Handler {
    fn handle(&self, path: &str) -> String;
}

impl Router {
    pub fn new() -> Self {
        Router { routes: HashMap::new() }
    }

    pub fn route(&self, path: &str) -> Option<String> {
        let h = self.routes.get(path)?;
        Some(h.handle(path))
    }
}

mod util {
    pub fn normalize(p: &str) -> String {
        p.trim_end_matches('/').to_string()
    }
}

macro_rules! route {
    ($r:expr) => {
        $r
    };
}

#[test]
fn routes_nothing() {
    let r = Router::new();
    assert!(r.route("/").is_none());
}
