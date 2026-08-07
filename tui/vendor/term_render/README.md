# **TermRender**
A high-performance, async-ready terminal UI library for Rust, built with flexibility and performance in mind. TermRender provides a modern widget system, efficient rendering, and comprehensive input handling for creating rich terminal applications.

# Features
* High Performance: Optimized rendering with dirty-rectangle tracking and efficient escape sequence handling

* Flexible Widget System: Build custom UI components with a trait-based approach

* Comprehensive Input: Full keyboard and mouse support with modifier keys

* Rich Text Support: ANSI colors, 24-bit RGB, and text styling

* Async Ready: Built on Tokio for modern async/await workflows

* Dynamic Layouts: Responsive layouts that adapt to terminal resizing

* Extensible: Easy to create custom widgets and renderers

# Quick Start
Add TermRender to your Cargo.toml:
```
toml
[dependencies]
term_render = "0.1.3"
tokio = { version = "1.47.1", features = ["full"] }
```

# Basic usage example:

```
use term_render::{self, event_handler::KeyCode};
use term_render::widget_impls::WidgetBuilder;

// this acts as the callback that is called every frame
// this is the entry point and any logic needs to branch out from here
fn app_callback(app: &mut term_render::App<AppData>, data: &mut AppData) -> Result<bool, ()> {
    // place app logic here
    if app.events.read().contains_key_code(KeyCode::Return) {
        return Ok(true);
    }
    if data.time.elapsed().as_secs_f64() > 15.0 {
        return Ok(true);
    }
    
    Ok(false)  // return true to exit the app
}

// defining the application data structure
// this can contain any data you want to use in the app callback
struct AppData {
    pub time: std::time::Instant,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() -> tokio::io::Result<()> {
    // creating an instance of the app
    let mut app = term_render::App::new()?;
    let data = AppData {
        time: std::time::Instant::now(),
    };
    
    // creating a new scene to attach widgets to
    let mut scene = term_render::widget::Scene::new();
    
    // creating a widget using the builder trait.
    let (widget, window) =
        term_render::widget_impls::StaticWidgetBuilder::<AppData>::builder(String::from("name"))
            .with_border(true)
            .with_renderer(Box::new(|_size, _position| {
                Some(vec![])
            }))
            .with_position((10, 10))
            .with_size((50, 10))
            .build(&app.area.read()).unwrap();
    scene.add_widget(widget, window, &mut *app.renderer.write()).unwrap();
    app.scene = Some(scene);
    
    // running the application with the callback, and provided data for state tracking.
    app.run(data, |data, app_instance| {
        app_callback(app_instance, data)
    }).await.unwrap();
    Ok(())
}
```

### Basic Explanation
The basis of a TermRender application is the `App` struct, which manages the terminal state, event handling, and rendering. You create an instance of `App`, set up your UI components (widgets), and then call `run` with a callback function that contains your application logic.
The callback function is called every frame, allowing you to update your application state and respond to events.
The `AppData` struct is a user-defined structure that holds any state you want to maintain across frames. In this example, it tracks the elapsed time since the application started.
The `App` instance optionally can have a `Scene`, which is a container for widgets. You can create widgets using the provided builders or implement your own by adhering to the `Widget` trait.

*When a name/string identifier is requested for a widget, it should be unique to that scene, as the backend renderer references `Window`'s (similar to a `Widget`, but lower level) not by the widget instance itself, but rather that string*

# Widget System
TermRender provides a flexible widget system:

## Built-in Widgets
* StaticWidget: Basic widget with custom rendering logic
* DynamicWidget: An interactable widget. In the future, more advanced versions will be implemented which will default as buttons or other such higher-level widgets.
* ButtonWidget: A simple button with automatic click handling.
* StaticTextWidget: Displays static text with styling options.
* TypingWidget: A customizable text input field.
* More coming soon!

* Creating Custom Widgets
  Implement the Widget trait to create custom components:

```
use term_render::{Widget, SendSync, event_handler::KeyParser};

// Where type C is the user defined app data state
struct MyCustomWidget<C> {
    // Your widget state
}

impl<C> Widget<C> for MyCustomWidget<C> {
    fn get_window_ref(&self) -> String {
        "my_custom_widget".to_string()  // note: each widget instance requires a unique identifyer
    }
    
    fn update_with_events(&mut self, events: &SendSync<KeyParser>) {
        // Handle events
    }
    
    fn update_render(&mut self, window: &mut Window, area: &Rect) {
        // Update rendering
    }
    
    // ... implement other required methods
}
```
# Performance Features
TermRender includes several optimizations:

* Dirty Rectangle Rendering: Only updates changed portions of the screen

* Efficient Event Handling: Minimal overhead input processing

* Smart Caching: Redundant render calls are avoided

* Async Rendering: Non-blocking UI updates

# Examples
Check out the examples directory for complete implementations (more coming soon):

- Run basic example
  cargo run --example basic

# Documentation
Full API documentation is available:

https://docs.rs/term_render/latest/term_render/

# Contributing
Contributions are welcome! Please feel free to submit pull requests, open issues, or suggest new features.

1. Fork the repository

2. Create your feature branch (git checkout -b feature/amazing-feature)

3. Commit your changes (git commit -m 'Add amazing feature')

4. Push to the branch (git push origin feature/amazing-feature)

5. Open a Pull Request

# License
This project is licensed under:

MIT license (LICENSE-MIT)

# Acknowledgments
* Inspired by Ratatui

* Built with performance and developer experience in mind

* Thanks to all contributors and the Rust community

*TermRender - Build beautiful terminal applications with Rust*