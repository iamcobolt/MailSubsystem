use crate::tui;

pub async fn run_tui(api_url: Option<String>) -> anyhow::Result<()> {
    tui::run_tui(api_url).await
}
