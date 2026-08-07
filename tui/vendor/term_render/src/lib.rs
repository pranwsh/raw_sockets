use std::io::Read;
/// Handling terminal events (key presses, mouse events, etc.)
/// This module provides functionality to parse and manage terminal input events.
/// The actual polling of events is handled externally.
pub mod event_handler;
/// Rendering to the terminal (drawing characters, managing screen buffer, etc.)
/// This module is often referred to as the backend rendering, or original rendering (
/// as it was the original component before the higher level abstractions were added).
/// Many of the nuances carried over to the higher level abstractions came from this module.
pub mod render;
/// Widgets and scene management (UI components, layout, etc.)
/// This module provides the building blocks for creating and managing UI components.
/// It includes a scene graph to manage the hierarchy and relationships between widgets.
pub mod widget;
/// Predefined widget implementations and builders (convenience functions for common widgets)
/// This module provides ready-to-use widget implementations and builders for common UI components.
/// It simplifies the process of creating and configuring widgets by providing default behaviors and properties.
pub mod widget_impls;
mod widget_button;
mod widget_dynamic;
mod widget_static;
mod widget_static_text;
mod widget_typing;

use crate::event_handler::KeyModifiers;
pub use term_render_macros::*;  // re-exporting the macros for easier use
pub use render::Colorize;  // making sure the colorize trait is included

// writing this out gets really verbose really quickly

/// A type alias for a thread-safe, shared, mutable reference to a value of type T.
/// This uses `Arc` for shared ownership and `RwLock` for interior mutability.'
/// The RwLock type comes from the `parking_lot` crate, which is a more efficient implementation
/// than the standard library's `std::sync::RwLock`. The `Arc` type used is still from the standard library.
/// This is used extensively throughout the application to manage shared state safely across threads.
/// This is also paired with the `send_sync!` macro to simplify the creation of such types.
pub type SendSync<T> = std::sync::Arc<parking_lot::RwLock<T>>;

/// Represents an internal error within the App struct/workings of lib.rs.
/// This is used to propagate errors from background tasks (rendering and event handling)
/// back to the main application loop, allowing for graceful shutdown and error reporting.
/// This error type is distinct from user-defined errors that may arise from the callback function.
#[derive(Debug)]
pub struct AppErr {
    details: String,
}

impl std::fmt::Display for AppErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AppErr: {}", self.details)
    }
}

impl AppErr {
    /// Create a new AppErr with the given details.
    /// The internal details are very limited and as such the creator
    /// should pre-format the details to be as informative as possible.
    pub fn new(details: &str) -> Self {
        AppErr { details: details.to_string() }
    }
}

/// The main application struct that combines rendering and event handling.
/// This will handle the background work, leaving the user to focus on the application logic.
/// The generic parameter C represents the application data type, which can be any type defined by the user.
/// This data is passed to the callback function every frame, allowing the user to maintain and update state.
/// The application data is stored separately from the App instance, but is linked from the instance.
/// This allows for a clear separation of concerns.
pub struct App<C> {
    /// The renderer instance, responsible for drawing to the terminal.
    /// This is wrapped in a SendSync to allow safe sharing and mutation across threads.
    /// The renderer is the core component that manages the terminal display.
    /// It handles the low-level details of rendering characters, managing the screen buffer,
    /// and optimizing updates to the terminal.
    pub renderer: SendSync<render::App>,
    /// The event handler instance, responsible for parsing and managing terminal input events.
    /// This is wrapped in a SendSync to allow safe sharing and mutation across threads.
    /// The event handler does not handle the polling of events itself; instead, it provides
    /// the necessary functionality to parse and manage events that are polled externally (
    /// this is all handled within the App struct).
    pub events: SendSync<event_handler::KeyParser>,
    /// The area of the terminal to render to.
    pub area: SendSync<render::Rect>,
    /// A flag to signal the application to exit.
    /// This is used to communicate between the main loop and the background tasks (rendering and event handling).
    /// When set to true, the application will begin the shutdown process.
    exit: SendSync<bool>,
    /// The scene graph managing the widgets and their layout.
    /// This is an optional field, as the application may not always have a scene.
    /// When present, it allows for the management of UI components and their relationships.
    /// The scene graph is responsible for updating and rendering the widgets based on the current state
    /// and events.
    pub scene: Option<widget::Scene<C>>,
}

impl<C> App<C> {
    /// Create a new instance of the App struct.
    /// This initializes the renderer and event handler.
    pub fn new() -> std::io::Result<Self> {
        let renderer = send_sync!(render::App::new()?);
        renderer.write().render(None);
        let events = send_sync!(event_handler::KeyParser::new());
        let (width, height) = renderer.read().get_terminal_size()?;
        
        Ok(Self {
            renderer,
            events,
            area: send_sync!(render::Rect { width, height }),
            exit: send_sync!(false),
            scene: None,
        })
    }
    
    /// Run the application with the provided callback function.
    /// This function sets up the necessary tasks for rendering and event handling,
    /// and enters the main loop where the provided callback function is called every frame.
    /// The callback function should return a `Result<bool, T>`, where the bool indicates whether to
    /// exit the application, and T is the error type.
    /// If the callback function returns an error, the application will exit and propagate the error.
    /// The application will also exit if Ctrl+C is detected (a fail-safe to ensure the application can be stopped).
    /// # Parameters
    /// - data: The application data to be passed to the callback function.
    /// - update_call_back: The callback function to be called every frame.
    /// # Example
    /// ```
    /// struct Data {
    ///     pub counter: u32,
    ///     // other fields...
    /// }
    /// let data = Data { counter: 0 };
    /// let mut app = term_render::App::new().unwrap();
    /// app.run(data, |data, app_instance| {
    ///     // the application data can be mutated here to track state
    ///     data.counter += 1;
    ///     // application logic here
    ///     Ok(false)  // return true to exit the app
    /// }).await.unwrap();
    /// ```
    pub async fn run<T: Sized + std::fmt::Debug>(&mut self, data: C, update_call_back: fn(&mut C, &mut App<C>) -> Result<bool, T>) -> Result<(), T> {
        self.renderer.write().render(None);
        // it seems that adding a back wall seems to fix the initialization rendering bug? Odd, but works
        // no idea why it's only a problem here but in past projects it never was
        let mut window = render::Window::new((1, 1), 0, (self.area.read().width, self.area.read().height));
        window.hide();
        self.renderer.write().add_window(window, String::from("null_window_back_wall_unique"), vec![]);
        self.renderer.write().render(None);

        let terminal_size_change = send_sync!(true);
        let terminal_size_change_clone = terminal_size_change.clone();

        let renderer_clone = self.renderer.clone();
        let (sender, receiver) = crossbeam::channel::bounded(10);
        let area_clone = self.area.clone();
        let exit_clone = self.exit.clone();
        let render_handle: tokio::task::JoinHandle<Result<(), AppErr>> = tokio::spawn( async move {
            Self::render((renderer_clone, receiver), area_clone, exit_clone, terminal_size_change_clone).await?;
            Ok(())
        });
        let exit_clone = self.exit.clone();
        let events_clone = self.events.clone();
        let events_handle = tokio::spawn( async move {
            Self::handle_events(exit_clone, events_clone).await;
        });
        match self.running_loop(data, update_call_back, sender, terminal_size_change).await {
            Err(e) => {
                println!("Error in running loop: {:?}", e);
            },
            Ok(_) => {},
        }
        
        //println!("Checking for errors");
        let mut error = false;
        *self.exit.write() = true;  // signal the tasks to exit
        match events_handle.await {
            Ok(_) => {},
            Err(e) => {
                println!("Error in event handling task: {:?}", e);
                error = true;
            },
        }
        match render_handle.await {
            Ok(Err(e)) => {
                println!("App Error in rendering task: {:?}", e);
                error = true;
            },
            Ok(_) => {},
            Err(e) => {
                println!("Error in rendering task: {:?}", e);
                error = true;
            },
        }

        if error {
            // panicking since the user type isn't known at compile time and can't be easily returned internally
            // panicking is necessary to ensure the app during drop doesn't clear the screen
            panic!("An error occurred during execution, see above for details.");
        }

        if let Some(sender) = self.renderer.write().concluded_sender.take() {
            match sender.send(()) {
                Ok(_) => {},
                Err(e) => {
                    // Same reason for panicking as above
                    panic!("Unable to call channel sender: {:?}", e);
                }
            }
        }

        Ok(())
    }
    
    /// The main loop for the application.
    /// This loop continuously calls the update callback function and checks for exit conditions.
    /// If the callback function returns true or if Ctrl+C is detected, the loop exits.
    /// If the callback function returns an error, the loop exits and the error is propagated.
    /// This function purely handles the logic of the application, while rendering and event handling are managed in separate tasks.
    /// # Parameters
    /// - data: The mutable application data to be passed to the callback function.
    /// - update_call_back: The callback function to be called every frame.
    /// - sender: A channel sender to signal the rendering task to update.
    /// - terminal_size_change: A flag to indicate if the terminal size has changed.
    /// # Returns
    /// - Result<(), AppErr>: Returns Ok(()) if the loop exits normally, or an AppErr if an error occurs.
    async fn running_loop<T: Sized + std::fmt::Debug>(&mut self,
                                                         mut data: C,
                                                         update_call_back: fn(&mut C, &mut App<C>) -> Result<bool, T>,
                                                         sender: crossbeam::channel::Sender<bool>,
                                                         terminal_size_change: SendSync<bool>
    ) -> Result<(), AppErr> {
        loop {
            // quick sleep to keep the events up-to-date enough
            tokio::time::sleep(tokio::time::Duration::from_secs_f64(0.01)).await;
            let result = update_call_back(&mut data, self);
            match result {
                Ok(should_exit) => {
                    let events_read = self.events.read();
                    // making sure there is some safety in case the user messed up something
                    if should_exit || (events_read.contains_modifier(KeyModifiers::Control) && events_read.contains_char('c')) {  break;  }
                },
                Err(e) => {
                    println!("Error in update callback: {:?}", e);
                    *self.exit.write() = true;  // signal the tasks to exit
                    break;
                },
            }
            
            // updating the scene
            if let Some(mut scene) = self.scene.take() {
                // updating all widgets' states based on the events and their rendered windows
                match scene.update_all_widgets(self, &mut data) {
                    Err(e) => {
                        *self.exit.write() = true;  // signal the tasks to exit
                        return Err(AppErr::new(&format!("Failed to update widgets in scene: {:?}", e)));
                    },
                    _ => {},
                }
                
                if *terminal_size_change.read() {
                    scene.force_update_all_widgets(&mut *self.renderer.write());
                }
                self.scene = Some(scene);
            }
            
            // updating the back wall
            let mut render_write = self.renderer.write();
            let win = render_write.get_window_reference_mut(String::from("null_window_back_wall_unique"));
            win.resize((self.area.read().width, self.area.read().height));
            drop(render_write);  // dropping it to prevent deadlocks
            
            self.events.write().clear_events();
            
            // if any background processes throw an error, exit will be set to true (otherwise, only this loop should set exit to true)
            if *self.exit.read() {  break;  }
            
            // updating the render (keeping it in sync)
            if !sender.is_full() {
                match sender.send(true) {
                    Ok(_) => {},
                    Err(e) => {
                       return Err(AppErr::new(&format!("Failed to send render sync on channel: {:?}", e)));
                    }
                }
            }
        }
        *self.exit.write() = true;
        Ok(())
    }
    
    /// Handle a single event from stdin.
    /// This function reads from stdin, parses the input, and updates the event handler.
    fn event_handling(parser: &mut vte::Parser, buffer: &mut [u8; 128], stdin: &mut std::io::Stdin, events: &SendSync<event_handler::KeyParser>) {
        let result = stdin.read(buffer);
        if let Ok(n) = result {
            events.write().bytes = n;
            if n == 1 && buffer[0] == 0x1B {
                events.write().key_events.insert(event_handler::KeyCode::Escape, true);
            } else {
                parser.advance(&mut *events.write(), &buffer[..n]);
            }
        }
    }
    
    // event handling task (runs a loop that reads from stdin and parses the input)
    // if this panics, it will set exit to true (signaling the app to shut down) and the main loop will exit
    
    /// Handle events in a separate thread.
    /// This function spawns a new thread that continuously reads from stdin and processes events.
    async fn handle_events(exit: SendSync<bool>, events: SendSync<event_handler::KeyParser>) {
        let events = events.clone();
        let exit_clone = exit.clone();
        // can't manually clean it up...
        let _result_handle = std::thread::spawn(
            move || {
                let events = events;
                let mut parser = vte::Parser::new();
                let mut buffer = [0; 128];
                let mut stdin = std::io::stdin();
                loop {
                    Self::event_handling(&mut parser, &mut buffer, &mut stdin, &events);
                    if *exit_clone.read() { break; }
                }
            }
        );
    }
    
    /// Handles rendering for a single frame.
    async fn render_handling(renderer: &SendSync<render::App>, area: &SendSync<render::Rect>, terminal_size_change: &SendSync<bool>) -> Result<(), AppErr> {
        let ar = match renderer.read().get_terminal_size() {
            Err(e) => {
                return Err(AppErr::new(&format!("Failed to get terminal size: {:?}", e)));
            },
            Ok(size) => size,
        };
        *terminal_size_change.write() = area.read().width != ar.0 || area.read().height != ar.1;
        *area.write() = render::Rect {
            width: ar.0,
            height: ar.1,
        };
        renderer.write().render(Some((area.read().width, area.read().height)));
        Ok(())
    }
    
    // rendering task (runs a loop that continuously renders the terminal)
    // if this panics, it will set exit to true (signaling the app to shut down) and the main loop will exit
    
    /// Handles rendering for the duration of the application.
    /// This function runs as an asynchronous task.
    async fn render(renderer: (SendSync<render::App>,
                               crossbeam::channel::Receiver<bool>),
                               area: SendSync<render::Rect>,
                               exit: SendSync<bool>,
                               terminal_size_change: SendSync<bool>
    ) -> Result<(), AppErr> {
        let exit_clone = exit.clone();
        let result_handle: tokio::task::JoinHandle<Result<(), AppErr>> = tokio::spawn(async move {
            loop {
                Self::render_handling(&renderer.0, &area, &terminal_size_change).await?;
                if *exit_clone.read() {  break;  }
                match renderer.1.recv() {
                    // the if is necessary to prevent errors whenever exiting (this would wait for a non-existent signal)
                    // no real errors or important ones should be sent in that tiny period of time
                    Err(e) if !*exit_clone.read() => { return Err(AppErr::new(&format!("Failed to receive render sync on channel: {:?}", e))); }  // channel disconnected, exit the loop
                    _ => {},
                }
            } Ok(())
        });
        let result = result_handle.await;
        match result {
            Ok(Err(e)) => {  // an app error vs. generic one
                *exit.write() = true;  // signal the tasks to exit
                return Err(e);
            },
            Ok(_) => {},
            Err(e) => {
                println!("Error in rendering: {:?}", e);
                *exit.write() = true;  // signal the tasks to exit
            },
        } Ok(())
    }
}

