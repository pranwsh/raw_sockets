use crate::widget_impls::*;
use crate::widget::*;

/// Builder for creating StaticWidget instances with a fluent interface.
/// Maintains configuration state until build() is called to create the actual widget.
/// `TypingWidgetBuilder` is an example of an implementation of `WidgetBuilder`, where
/// the struct doesn't implement `Widget`.
pub struct TypingWidgetBuilder<C> {
    /// The unique name identifier for the widget.
    name: String,
    /// The z-index depth of the widget; higher values render on top of lower ones.
    depth: Option<u16>,
    /// Whether the widget should have a border.
    border: bool,
    /// The title of the widget, if any.
    title: Option<String>,
    /// The size and position configuration for the widget.
    pub size_and_position: SizeAndPosition,
    /// The custom render function for the widget, if any.
    pub render_function: Option<Box<dyn Fn((u16, u16), (u16, u16), &[&str; 2], bool) -> Option<Vec<crate::render::Span>>>>,
    /// The index of the parent widget in the scene graph, if any.
    parent: Option<usize>,
    
    update_handler: Option<Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>>,
    
    __phantom: std::marker::PhantomData<C>,
}

/// Implementations for the methods in `WidgetBuilder`.
impl<C: 'static> WidgetBuilder<C> for TypingWidgetBuilder<C> {
    /// Constructs a `TypingWidget`, an implementor of `Widget`, given the parameters.
    /// Validates that size and position are non-zero before creating the widget.
    /// The method takes in a reference to the terminal's current area/size.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{TypingWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    /// let (widget, window) = TypingWidgetBuilder::<AppData>::builder(String::new())
    ///     .build(&Rect::default())  // replace &Rect with the actual terminal size (such as `&app.area.read()`)
    ///     .expect("Invalid widget position or size.");
    /// ```
    fn build(mut self, display_area: &crate::render::Rect) -> Result<(Box<dyn Widget<C>>, crate::render::Window), WidgetBuilderError> {
        let (position, size) = self.size_and_position.get_size_and_position(display_area);
        if size.0 == 0 || size.1 == 0 || position.0 == 0 || position.1 == 0 {
            return Err(WidgetBuilderError { details: String::from("Position and/or size cannot be zero when building a new widget or window.") })
        }
        let depth = self.depth.as_ref().unwrap_or(&0u16);
        let mut window = crate::render::Window::new(position, *depth, size);
        if self.border {  window.bordered();  }
        if let Some(title) = &self.title {  window.titled(title.clone());  }
        Ok((Box::new(TypingWidget::<C> {
            children: vec![],
            name: self.name,
            parent_index: self.parent,
            size_and_position: self.size_and_position,
            render_function: self.render_function,
            update_handler: self.update_handler,
            typed_text: String::new(),
            selected: false,
            cursor_pos: 0,
            __phantom: std::marker::PhantomData,
        }), window))
    }
    
    /// Sets the widget's fixed position (static layout).
    /// Retrains the dynamic proportions of any dynamic positioning configuration already in place.
    fn with_position(mut self, position: (u16, u16)) -> Self {
        self.size_and_position.position_offset = (position.0 as i16, position.1 as i16);
        self
    }
    
    /// Sets the widget's fixed position (static layout).
    /// Retrains the dynamic proportions of any dynamic size configuration already in place.
    fn with_size(mut self, size: (u16, u16)) -> Self {
        self.size_and_position.size_offset = (size.0 as i16, size.1 as i16);
        self
    }
    
    /// Configures dynamic positioning based on terminal size with a fixed offset.
    fn with_dynamic_position(mut self, position_offset: (i16, i16), position_area_percent: (f32, f32)) -> Self {
        self.size_and_position.position_offset = position_offset;
        self.size_and_position.position_area_percent = position_area_percent;
        self
    }
    
    /// Configures dynamic sizing based on terminal size with a fixed offset.
    fn with_dynamic_size(mut self, size_offset: (i16, i16), size_area_percent: (f32, f32)) -> Self {
        self.size_and_position.size_offset = size_offset;
        self.size_and_position.size_area_percent = size_area_percent;
        self
    }
    
    /// Sets whether the widget should have a border. By default, all widgets are borderless.
    fn with_border(mut self, border: bool) -> Self {
        self.border = border;
        self
    }
    
    /// Sets the widget's title (displayed in border if enabled; invisible otherwise).
    fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }
    
    /// Assigns a depth to the widget. Higher values represent UI elements that are stacked further
    /// ontop of other elements. By default, the builder instance has a depth of None, which corresponds
    /// to 0 (root depth). This is fine as long as it isn't trying to appear ontop of other elements, such
    /// as for a pop-up.
    fn with_depth(mut self, depth: u16) -> Self {
        self.depth = Some(depth);
        self
    }
    
    /// The type representing the renderer closure.
    type RendererType = Box<dyn Fn((u16, u16), (u16, u16), &[&str; 2], bool) -> Option<Vec<crate::render::Span>>>;
    /// Sets the rendering closure that generates content for the widget.
    /// The closure receives size and position parameters and returns an optional vector of type `Span`.
    /// By default, there is no renderer, leaving the widget empty (apart from stylization like a border or title).
    /// The closure is a boxed closure that takes in `(size: (u16, u16), position: (u16, u16), typed_text: &[&str; 2])`. This closure
    /// can capture local context to allow for easier dynamic variations between widgets with minimal boilerplate.
    /// #Example:
    /// ```
    /// use term_render::widget_impls::{TypingWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    ///
    /// // the closure can capture local variables to reduce boilerplate
    /// let closure = Box::new(move |size, position, typed_text| {
    ///     None  // this will leave the widget un-updated (it will default to its cache and assume no updates are necessary unless other events occur)
    /// });
    /// let (widget, window) = TypingWidgetBuilder::<AppData>::builder(String::new())
    ///     .with_renderer(closure)
    ///     .build(&Rect::default())
    ///     .unwrap();
    /// ```
    fn with_renderer(mut self, renderer: Self::RendererType) -> Self {
        self.render_function = Some(renderer);
        self
    }
    
    /// Generates a new builder instance with a provided unique name identifier.
    /// By default, size_and_position are no compatible and require the user to
    /// provide them using the other `WidgetBuilder` trait functions.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{TypingWidgetBuilder, WidgetBuilder};
    /// let builder = TypingWidgetBuilder::<AppData>::builder(String::from("Widget Name"));
    /// ```
    fn builder(name: String) -> Self {
        Self {
            name,
            depth: None,
            size_and_position: SizeAndPosition::default(),
            render_function: None,
            border: false,
            title: None,
            parent: None,
            update_handler: None,
            __phantom: std::marker::PhantomData,
        }
    }
    
    /// Sets the SizeAndPosition configuration directly.
    fn with_sap(mut self, sap: SizeAndPosition) -> Self {
        self.size_and_position = sap;
        self
    }

    type FunctionType = Option<Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>>;
    /// The box itself is basically static, however the text being typed is dynamic and will call the
    /// callback closure to allow for state changes and other actions.
    fn with_update_handler(mut self, handler: Self::FunctionType) -> Self {
        // static widgets don't need an update handler
        self.update_handler = handler;
        self
    }
    
    /// Sets the parent widget index for this widget, if any.
    /// By default, the parent is None, indicating a root node.
    fn with_parent(mut self, parent: Option<usize>) -> Self {
        self.parent = parent;
        self
    }
    
    /// Builds the widget and adds it to the provided scene, returning the new widget's index in the scene graph.
    /// This method combines the `build` and `scene.add_widget` calls into one for convenience.
    /// If building the widget fails, an error is returned instead.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{TypingWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    /// let mut app = term_render::App::new().unwrap();
    /// let mut scene = term_render::widget::Scene::new();
    /// let widget_index = TypingWidgetBuilder::<AppData>::builder(String::from("My Widget"))
    ///     .with_position((5, 5))
    ///     .with_size((20, 10))
    ///     .add_to_scene(&mut app, &mut scene)
    ///     .expect("Failed to build and add widget to scene.");
    /// ```
    fn add_to_scene(self, app: &mut crate::App<C>, scene: &mut Scene<C>) -> Result<usize, WidgetErr> {
        if let Ok((widget, window)) = self.build(&app.area.read()) {
            scene.add_widget(widget, window, &mut *app.renderer.write())
        } else {
            Err(WidgetErr::new("Failed to build and add widget to scene."))
        }
    }
}

/// A widget that renders static content using a provided closure (i.e.
/// a title box or description).
/// Suitable for content that doesn't change frequently or in response to events.
/// `TypingWidgetBuilder` is the associated builder for creating instances of this widget.
/// The generic parameter C represents the application data type, which can be any type defined by the user.
pub struct TypingWidget<C> {
    /// The indices of child widgets in the scene graph.
    children: Vec<usize>,

    /// The unique name identifier for the widget. The rendering backend
    /// relies on `String` names instead of widgets.
    name: String,

    /// The index of the parent widget in the scene graph, if any (None would
    /// indicate the root node).
    parent_index: Option<usize>,
    
    // this should be an easy, lightweight, and changeable way to get the size and position
    // it needs to be light enough to be changed to something new if the state of the app changes
    // (such as a menu opening and shifting things).
    /// Configuration for the widget's size and position, supporting both static and dynamic layouts.
    pub size_and_position: SizeAndPosition,
    
    // takes the size and position in, and returns the vector of spans to render
    // this is a function object, allowing for capturing of state if desired
    /// Optional closure that generates the widget's rendered content based on size and position.
    pub render_function: Option<Box<dyn Fn((u16, u16), (u16, u16), &[&str; 2], bool) -> Option<Vec<crate::render::Span>>>>,
    
    /// Optional closure that handles updates to the widget during event processing.
    /// This closure can modify the widget or application state as needed.
    update_handler: Option<Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>>,
    
    /// The text that has been typed into the widget so far.
    pub typed_text: String,
    
    /// Indicates whether the widget is currently selected (focused for input).
    pub selected: bool,
    
    pub cursor_pos: usize,

    __phantom: std::marker::PhantomData<C>,
}

impl<C> TypingWidget<C> {
    /// Creates a new TypingWidget and its associated window.
    /// # Arguments
    /// * `name` - Unique identifier for the widget
    /// * `size_and_position` - Configuration for size and positioning
    /// * `render_function` - Optional closure that renders content given size and position
    /// * `depth` - Z-index for rendering order
    /// * `display_area` - Current terminal area for initial size/position calculation
    /// # Returns
    /// Result containing the widget and window, or error if dimensions are invalid
    ///
    /// *When possible, an implementation of `WidgetBuilder` should be used instead, both for safety,
    /// simplicity, and consistency.*
    pub fn new(name: String,
               mut size_and_position: SizeAndPosition,
               render_function: Option<Box<dyn Fn((u16, u16), (u16, u16), &[&str; 2], bool) -> Option<Vec<crate::render::Span>>>>,
               depth: u16,
               display_area: &crate::render::Rect,
    ) -> Result<(TypingWidget<C>, crate::render::Window), WidgetErr> {
        let (size, position) = size_and_position.get_size_and_position(display_area);
        if size.0 == 0 || size.1 == 0 || position.0 == 0 || position.1 == 0 {
            return Err(WidgetErr::new("Size or position cannot be zero"))
        }
        let window = crate::render::Window::new(position, depth, size);
        
        let widget = TypingWidget::<C> {
            children: vec![],
            name,
            parent_index: None,
            size_and_position,
            render_function,
            update_handler: None,
            typed_text: String::new(),
            selected: false,
            cursor_pos: 0,
            __phantom: std::marker::PhantomData,
        };
        
        Ok((widget, window))
    }
}

/// Implementation of the methods for TypingWidget
impl<C> Widget<C> for TypingWidget<C> {
    /// Returns the widget's name as an identifier. The rendering backend relies
    /// on `String` names instead of widgets.
    fn get_window_ref(&self) -> String {
        self.name.clone()
    }
    
    // for handling updates (a static widget would just have this empty)
    /// Handles event updates. However, compared to the other widgets, this one
    /// doesn't directly act to modify the widget, but rather to respond to changes in text input.
    fn update_with_events(&mut self, data: &mut C, app: &mut crate::App<C>, scene: &mut Scene<C>) {
        // checking if the box is being selected, or unselected
        if let Some(event) = &app.events.read().mouse_event {
            if event.event_type == crate::event_handler::MouseEventType::Left {
                self.selected = self.is_collided(event.position) &&
                    !scene.is_click_blocked_all(scene.get_widget_index(self.get_window_ref())
                    .unwrap_or(0), event.position, &*app).unwrap_or(false);
            }
        }
        
        // actually handling text input if selected
        let events = app.events.read();
        if self.selected && !events.contains_modifier(crate::event_handler::KeyModifiers::Control) &&
            !events.contains_modifier(crate::event_handler::KeyModifiers::Command)
        {
            for char in &events.char_events {
                self.typed_text.insert(self.cursor_pos, *char);
                self.cursor_pos += 1;
            }
            
            // handling left, right and backspace
            if events.contains_key_code(crate::event_handler::KeyCode::Delete) && self.cursor_pos > 0 {
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
                self.typed_text.remove(self.cursor_pos);
            }
            if events.contains_key_code(crate::event_handler::KeyCode::Left) {
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
            }
            if events.contains_key_code(crate::event_handler::KeyCode::Right) {
                self.cursor_pos = usize::min(self.cursor_pos + 1, self.typed_text.len());
            }
        } drop(events);  // making sure there isn't a deadlock
        
        if let Some(update_handler) = self.update_handler.take() {
            update_handler(self, data, app, scene);
            self.update_handler = Some(update_handler);
        }
    }
    
    /// Updates the widget's rendering based on current size and position.
    /// Called automatically during render passes.
    /// If `Some(render_closure)` is provided, that closure will be called.
    /// If the closure returns `Some(Vec<Span>)`, then the rendered content will be set as such.
    fn update_render(&mut self, window: &mut crate::render::Window, area: &crate::render::Rect) -> bool {
        // only needs to change with size
        let (size, position) = self.size_and_position.get_size_and_position(area);
        window.resize(size);
        window.r#move(position);
        if let Some(render_function) = &self.render_function {
            let typed = &[self.typed_text.get(0..self.cursor_pos).unwrap_or(""),
                self.typed_text.get(self.cursor_pos..).unwrap_or("")];
            if let Some(render) = render_function(size, position, &typed, self.selected) {
                return window.try_update_lines(render);
            }
        } false
    }
    
    /// Returns the indices of child widgets in the scene graph.
    fn get_children_indexes(&self) -> Vec<usize> {
        self.children.clone()
    }
    
    /// Adds a child widget index to this widget.
    fn add_child_index(&mut self, index: usize) {
        self.children.push(index);
    }
    
    /// Removes a child widget index from this widget
    fn remove_child_index(&mut self, index: usize) {
        self.children.remove(index);
    }
    
    /// Clears all child widget indices from this widget
    fn clear_children_indexes(&mut self) {
        self.children.clear();
    }
    
    /// Returns the parent widget index if one exists, otherwise None.
    fn get_parent_index(&self) -> Option<usize> {
        self.parent_index
    }
    
    /// Sets the parent widget index for this widget, or None for a root node.
    fn set_parent_index(&mut self, index: Option<usize>) {
        self.parent_index = index;
    }
    
    /// Determines if a given position collides with the widget's area.
    fn is_collided(&self, position: (u16, u16)) -> bool {
        let (size, pos) = self.size_and_position.get_last();
        position.0 >= pos.0 && position.0 < pos.0 + size.0 && position.1 >= pos.1 && position.1 < pos.1 + size.1
    }
}
