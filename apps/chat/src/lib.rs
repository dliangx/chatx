slint::include_modules!();

fn ui() -> MainWindow {
    MainWindow::new().unwrap()
}

pub fn main() {
    let ui = ui();
    ui.run().unwrap();
}
