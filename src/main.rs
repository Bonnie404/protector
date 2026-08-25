use chrono::Local;
use protector::task::format_countdown;
use protector::tray::{
    self,
    menu_model::{Action, MenuItem, MenuModel},
    Command, UiState,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let menu = MenuModel::new(vec![
        MenuItem::radio(
            1,
            "Design review   14:00 \u{2013} 15:30",
            true,
            Action::SelectTask("demo".into()),
        ),
        MenuItem::separator(2),
        MenuItem::command(3, "Quit", Action::Quit),
    ]);
    let (ui_tx, ui_rx) = tokio::sync::watch::channel(UiState {
        label: "starting\u{2026}".into(),
        attention: false,
        menu: menu.clone(),
    });
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(32);
    let _conn = tray::run_tray(ui_rx, cmd_tx.clone()).await?;

    let end = Local::now() + chrono::Duration::minutes(3);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let remaining = (end - Local::now()).num_seconds();
            let _ = ui_tx.send(UiState {
                label: format!("{} \u{b7} Design review", format_countdown(remaining)),
                attention: remaining < 0,
                menu: menu.clone(),
            });
        }
    });

    while let Some(cmd) = cmd_rx.recv().await {
        if let Command::MenuClicked(id) = cmd {
            println!("clicked {id}");
            if id == 3 {
                break;
            }
        }
    }
    Ok(())
}
