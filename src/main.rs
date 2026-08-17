use eframe::egui;
use roche_workstation::ui::desktop::DesktopApp;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Roche AI Workstation")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([980.0, 640.0]),
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "roche-ai-workstation",
        options,
        Box::new(|cc| Ok(Box::new(DesktopApp::new(cc)))),
    )
}
