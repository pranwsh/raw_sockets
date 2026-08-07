use term_render::widget_impls::{WidgetBuilder};
use term_render::{self, event_handler::KeyCode};
use term_render::render::{Colorize, ColorType};
use term_render::widget::{Widget, Scene};
use term_render::render::Span;
use term_render::color;

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
    // just an example field to show how to use the data struct
    // in this case, the example is being used to close the app after a set amount of time has elapsed
    pub time: std::time::Instant,
}

// handles the behavior of the secondary button that spawns another popup
fn secondary_button_behavior(
    widget: &mut dyn Widget<AppData>,
    _data: &mut AppData,
    app: &mut term_render::App<AppData>,
    scene: &mut Scene<AppData>,
    state: &term_render::widget_impls::ButtonState,
) {
    let widget_index = scene.get_widget_index(widget.get_window_ref()).unwrap_or(0);
    let mut pressed = state == &term_render::widget_impls::ButtonState::Pressed(term_render::event_handler::MouseEventType::Left);
    if let Some(event) = &app.events.read().mouse_event {
        pressed &= !scene.is_click_blocked(widget_index, event.position).unwrap_or(false);
    }
    if !pressed {  return  }
    if widget.get_children_indexes().is_empty() {  // in the case of this scene, the only children of a widget would be the unique popup
        // creating another popup, this time as a static textbox showing different Widget implementations and how they can be nested
        term_render::widget_impls::StaticWidgetBuilder::<AppData>::builder(String::from("popup_final"))
            .with_border(true)
            .with_renderer(Box::new(|_size, _position| {
                Some(vec![Span::from_tokens(vec![color!("This is another popup!", Red)])])
            }))
            .with_dynamic_position((18, 7), (0.1, 0.1))
            .with_dynamic_size((35, 14), (0.1, 0.1))
            .with_depth(2)
            .with_parent(scene.get_widget_index(String::from("popup")))
            .add_to_scene(app, scene)
            .unwrap();
    } else {
        scene.remove_widget_ref(String::from("popup_final"), &mut *app.renderer.write()).unwrap();
    }
}

// handles the behavior of the main button that spawns a popup
fn base_button_behavior(
    widget: &mut dyn Widget<AppData>,
    _data: &mut AppData,
    app: &mut term_render::App<AppData>,
    scene: &mut Scene<AppData>,
) {
    // if the widget was pressed, create a popup widget as its child
    let widget_index = scene.get_widget_index(widget.get_window_ref()).unwrap_or(0);
    let pressed = if let Some(event) = &app.events.read().mouse_event {
        if event.event_type == term_render::event_handler::MouseEventType::Left &&
            event.state == term_render::event_handler::MouseState::Press &&
            widget.is_collided(event.position) {
            !scene.is_click_blocked(widget_index, event.position).unwrap_or(false)
        } else {  false  }
    } else {  false  };
    if !pressed {  return  }
    if widget.get_children_indexes().is_empty() {  // in the case of this scene, the only children of a widget would be the unique popup
        // creating another widget, but this time as a button type which shows how it can simplify the user end code
        term_render::widget_impls::ButtonWidgetBuilder::<AppData>::builder(String::from("popup"))
            .with_border(true)
            .with_renderer(Box::new(|_size, _position, state| {
                // rendering different text colors based on the button state (this could definitely be done better with less code, but this is just an example)
                match state {
                    term_render::widget_impls::ButtonState::Pressed(_) => {
                        Some(vec![Span::from_tokens(vec![color!("This is a popup!", Red)])])
                    },
                    term_render::widget_impls::ButtonState::Held(_) => {
                        Some(vec![Span::from_tokens(vec![color!("This is a popup!", BrightRed)])])
                    },
                    term_render::widget_impls::ButtonState::Released(_) => {
                        Some(vec![Span::from_tokens(vec![color!("This is a popup!", Yellow)])])
                    },
                    term_render::widget_impls::ButtonState::Hovered => {
                        Some(vec![Span::from_tokens(vec![color!("This is a popup!", BrightWhite, Bold)])])
                    },
                    term_render::widget_impls::ButtonState::Normal => {
                        Some(vec![Span::from_tokens(vec![color!("This is a popup!", White)])])
                    },
                }
            }))
            .with_update_handler(Box::new(|widget, _data, app: &mut term_render::App<AppData>, scene, state| {
                // basic logic could be placed within the closure and/or a separate function could be called
                secondary_button_behavior(widget, _data, app, scene, state);
            }))
            .with_position((15, 5))
            .with_size((30, 12))
            .with_depth(1)
            .with_parent(scene.get_widget_index(String::from("button")))
            .add_to_scene(app, scene)
            .unwrap();
    } else {
        scene.remove_widget_ref(String::from("popup"), &mut *app.renderer.write()).unwrap();
    }
}

// the main function has to be async to enable the use of async tasks further down the road (mainly under the hood)
#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() -> tokio::io::Result<()> {
    // creating the application instance (this will initialize the renderer and event handler)
    let mut app = term_render::App::new()?;
    
    // defining the application data (stored separately from the app instance, but linked from the instance)
    let data = AppData {
        time: std::time::Instant::now(),
    };
    
    // creating a new scene which will have widgets attached to it
    let mut scene = Scene::new();
    
    // creating another widget that is just static text
    // because this is static, I'll be using it as the root level widget
    term_render::widget_impls::StaticTextWidgetBuilder::<AppData>::builder(String::from("text"))
        .with_border(true)
        .with_renderer(vec![
            Span::from_tokens(vec![color!("This is a text!")]),
        ])
        .with_dynamic_position((0, 0), (0.5, 0.5))
        .with_size((35, 3))
        .add_to_scene(&mut app, &mut scene)
        .unwrap();
    
    // creating a random typing field to show how it can be used
    term_render::widget_impls::TypingWidgetBuilder::<AppData>::builder(String::from("Typing box"))
        .with_border(true)
        .with_renderer(Box::new(|_size, _position, content, selected| {
            Some(vec![Span::from_tokens(vec![color!(match !(content[0].is_empty() && content[1].is_empty()) {
                true if selected => format!("{}|{}", content[0], content[1]),  // showing the current content with a cursor at the end
                true => format!("{}{}", content[0], content[1]),
                // a placeholder text when empty to indicate where to type
                false => String::from("Type here..."),
            }, Green)])])
        }))
        .with_dynamic_position((18, 3), (0.25, 0.5))
        .with_size((35, 3))
        .add_to_scene(&mut app, &mut scene)
        .unwrap();
    
    // creating a widget that has the functionality to create more widgets based on user interactions
    term_render::widget_impls::DynamicWidgetBuilder::<AppData>::builder(String::from("button"))
        .with_border(true)
        .with_renderer(Box::new(|_size, _position| {
            Some(vec![Span::from_tokens(vec![color!("I am thing!")])])
        }))
        .with_position((10, 10))
        .with_size((50, 10))
        .with_update_handler(Box::new(|widget, _data, app: &mut term_render::App<AppData>, scene| {
            // basic logic could be placed within the closure and/or a separate function could be called
            // using a function does allow for reusability if needed, and can help keep the closure cleaner
            // a mix of the two is also possible
            base_button_behavior(widget, _data, app, scene);
        }))
        .with_parent(scene.get_widget_index(String::from("text")))
        .add_to_scene(&mut app, &mut scene)
        .unwrap();
    
    // attaching the scene to the main app
    app.scene = Some(scene);
    
    // running the application with the provided callback function
    app.run(data, |data, app_instance: &mut term_render::App<AppData>| {
        app_callback(app_instance, data)
    }).await.unwrap();
    
    Ok(())
}
