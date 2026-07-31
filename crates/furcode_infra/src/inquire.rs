use anyhow::Result;
use furcode_app::UserInfra;
use furcode_select::FurcodeWidget;

pub struct FurcodeInquire;

impl Default for FurcodeInquire {
    fn default() -> Self {
        Self::new()
    }
}

impl FurcodeInquire {
    pub fn new() -> Self {
        Self
    }

    async fn prompt<T, F>(&self, f: F) -> Result<Option<T>>
    where
        F: FnOnce() -> Result<Option<T>> + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(f).await?
    }
}

#[async_trait::async_trait]
impl UserInfra for FurcodeInquire {
    async fn prompt_question(&self, question: &str) -> Result<Option<String>> {
        let question = question.to_string();
        self.prompt(move || FurcodeWidget::input(&question).allow_empty(true).prompt())
            .await
    }

    async fn select_one<T: Clone + std::fmt::Display + Send + 'static>(
        &self,
        message: &str,
        options: Vec<T>,
    ) -> Result<Option<T>> {
        if options.is_empty() {
            return Ok(None);
        }

        let message = message.to_string();
        self.prompt(move || FurcodeWidget::select(&message, options).prompt())
            .await
    }

    async fn select_many<T: std::fmt::Display + Clone + Send + 'static>(
        &self,
        message: &str,
        options: Vec<T>,
    ) -> Result<Option<Vec<T>>> {
        if options.is_empty() {
            return Ok(None);
        }

        let message = message.to_string();
        self.prompt(move || FurcodeWidget::multi_select(&message, options).prompt())
            .await
    }
}
