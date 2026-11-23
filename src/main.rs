mod app;
#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(not(target_arch = "wasm32"))]
mod export;
mod gpu;
mod media;
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(any(test, target_arch = "wasm32"))]
mod ts;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use std::path::PathBuf;

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--export-360") {
        if args.len() != 3 {
            eprintln!("Usage: surround-view --export-360 input.ts output.mp4");
            std::process::exit(2);
        }
        let result = export::run_headless(PathBuf::from(&args[1]), PathBuf::from(&args[2]));
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    let initial_file = args.first().map(PathBuf::from);
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Surround View")
            .with_inner_size([1200.0, 820.0]),
        ..Default::default()
    };
    if let Err(error) = eframe::run_native(
        "Surround View",
        options,
        Box::new(|cc| Ok(Box::new(app::Player::new(cc, initial_file)?))),
    ) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;

    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|window| window.document()) else {
            return;
        };
        let Some(canvas) = document
            .get_element_by_id("surround-view")
            .and_then(|element| element.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        else {
            return;
        };
        let options = eframe::WebOptions {
            ..Default::default()
        };
        if let Err(error) = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(|cc| Ok(Box::new(app::Player::new(cc)?))),
            )
            .await
        {
            web_error(&format!("WebGPU initialization failed: {error:?}"));
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn web_error(message: &str) {
    if let Some(element) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("launch-error"))
    {
        element.set_text_content(Some(message));
    }
}
