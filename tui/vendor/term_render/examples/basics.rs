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

// the main function has to be async to enable the use of async tasks further down the road (mainly under the hood)
#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() -> tokio::io::Result<()> {
    // creating the application instance (this will initialize the renderer and event handler)
    let mut app = term_render::App::new()?;
    
    // defining the application data (stored separately from the app instance, but linked from the instance)
    let data = AppData {
        time: std::time::Instant::now(),
    };
    
    let mut scene = term_render::widget::Scene::new();
    
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
    
    // running the application with the provided callback function
    app.run(data, |data, app_instance| {
        app_callback(app_instance, data)
    }).await.unwrap();
    
    Ok(())
}
