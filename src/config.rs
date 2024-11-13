use dotenv::dotenv;
use std::env;

pub struct AppConfig {
    pub port: u16,
    pub host: String,
}

impl AppConfig {
    pub fn new() -> Self {
        dotenv().ok();

        Self {
            port: env::var("PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .expect("PORT must be a number"),
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
        }
    }
}
