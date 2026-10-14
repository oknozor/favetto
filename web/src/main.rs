//! Binary entry point for both the web and Android targets.

fn main() {
    dioxus::launch(favetto_web::app::App);
}
