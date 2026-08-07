use crate::widget_impls::*;
use crate::widget::*;

/// Builder for creating StaticTextWidget instances with a fluent interface.
/// Maintains configuration state until build() is called to create the actual widget.
/// `StaticTextWidgetBuilder` is an example of an implementation of `WidgetBuilder`, where
/// the struct doesn't implement `Widget`.
pub struct StaticTextWidgetBuilder<C> {
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
    pub render_text: Vec<crate::render::Span>,
    /// The index of the parent widget in the scene graph, if any.
    parent: Option<usize>,
    
    __phantom: std::marker::PhantomData<C>,
}

/// Implementations for the methods in `WidgetBuilder`.
impl<C: 'static> WidgetBuilder<C> for StaticTextWidgetBuilder<C> {
    /// Constructs a `StaticTextWidget`, an implementor of `Widget`, given the parameters.
    /// Validates that size and position are non-zero before creating the widget.
    /// The method takes in a reference to the terminal's current area/size.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{StaticTextWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    /// let (widget, window) = StaticTextWidgetBuilder::<AppData>::builder(String::new())
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
        Ok((Box::new(StaticTextWidget::<C> {
            children: vec![],
            name: self.name,
            parent_index: self.parent,
            size_and_position: self.size_and_position,
            render_text: self.render_text,
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
    
    /// The type representing the renderer content. This is different from other widgets
    /// as it is not a closure, but rather the actual content to render.
    type RendererType = Vec<crate::render::Span>;
    /// This renderer is unique, as instead of providing a render closure, the user provides
    /// the actual rendered content directly. This is because static text widgets don't need to
    /// change their content dynamically, and as such a closure is unnecessary overhead.
    /// This also means that once the widget sets the text, it will never change. The widget can
    /// change size and position, but the text will remain constant. If the text is meant to
    /// be centered, it will not retain that centering so a `StaticWidget` or `DynamicWidget`
    /// would be more appropriate.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{StaticTextWidgetBuilder, WidgetBuilder};
    /// use term_render::render::{Rect, Colorize, ColorType};
    /// use term_render::color;
    ///
    /// let (widget, window) = StaticTextWidgetBuilder::<AppData>::builder(String::new())
    ///     .with_renderer(vec![
    ///        term_render::render::Span::from_tokens(vec![
    ///             color!["Hello World!", Blue, Bold],
    ///         ])
    ///     ])
    ///     .build(&Rect::default())
    ///     .unwrap();
    /// ```
    fn with_renderer(mut self, renderer: Self::RendererType) -> Self {
        self.render_text = renderer;
        self
    }
    
    /// Generates a new builder instance with a provided unique name identifier.
    /// By default, size_and_position are no compatible and require the user to
    /// provide them using the other `WidgetBuilder` trait functions.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{StaticTextWidgetBuilder, WidgetBuilder};
    /// let builder = StaticTextWidgetBuilder::<AppData>::builder(String::from("Widget Name"));
    /// ```
    fn builder(name: String) -> Self {
        Self {
            name,
            depth: None,
            size_and_position: SizeAndPosition::default(),
            render_text: vec![],
            border: false,
            title: None,
            parent: None,
            __phantom: std::marker::PhantomData,
        }
    }
    
    /// Sets the SizeAndPosition configuration directly.
    fn with_sap(mut self, sap: SizeAndPosition) -> Self {
        self.size_and_position = sap;
        self
    }
    
    /// Static widgets do not respond to events, so this is a no-op that returns self.
    type FunctionType = Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>;
    fn with_update_handler(self, _handler: Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>) -> Self {
        // static widgets don't need an update handler
        self
    }
    
    /// Sets the parent widget index for this widget, if any.
    /// By default, the parent is None, indicating a root node.
    /// However, only one root node can exist at a given time in a scene graph.
    fn with_parent(mut self, parent: Option<usize>) -> Self {
        self.parent = parent;
        self
    }
    
    /// Builds the widget and adds it to the provided scene, returning the new widget's index in the scene graph.
    /// This method combines the `build` and `scene.add_widget` calls into one for convenience.
    /// If building the widget fails, an error is returned instead.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{StaticTextWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    /// let mut app = term_render::App::new().unwrap();
    /// let mut scene = term_render::widget::Scene::new();
    /// let widget_index = StaticTextWidgetBuilder::<AppData>::builder(String::from("My Widget"))
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
/// `StaticTextWidgetBuilder` is the associated builder for creating instances of this widget.
/// The generic parameter C represents the application data type, which can be any type defined by the user.
pub struct StaticTextWidget<C> {
    /// The indices of child widgets in the scene graph.
    children: Vec<usize>,
    
    /// The unique name identifier for the widget. The rendering backend
    /// relies on `String` names instead of widgets.
    name: String,
    
    /// The index of the parent widget in the scene graph, if any (None would
    /// indicate the root node, which there can only be one of).
    parent_index: Option<usize>,
    
    // this should be an easy, lightweight, and changeable way to get the size and position
    // it needs to be light enough to be changed to something new if the state of the app changes
    // (such as a menu opening and shifting things).
    /// Configuration for the widget's size and position, supporting both static and dynamic layouts.
    pub size_and_position: SizeAndPosition,
    
    // takes the size and position in, and returns the vector of spans to render
    // this is a function object, allowing for capturing of state if desired
    /// Optional closure that generates the widget's rendered content based on size and position.
    pub render_text: Vec<crate::render::Span>,
    
    __phantom: std::marker::PhantomData<C>,
}

impl<C> StaticTextWidget<C> {
    /// Creates a new StaticTextWidget and its associated window.
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
               render_text: Vec<crate::render::Span>,
               depth: u16,
               display_area: &crate::render::Rect,
    ) -> Result<(StaticTextWidget<C>, crate::render::Window), WidgetErr> {
        let (size, position) = size_and_position.get_size_and_position(display_area);
        if size.0 == 0 || size.1 == 0 || position.0 == 0 || position.1 == 0 {
            return Err(WidgetErr::new("Size or position cannot be zero"))
        }
        let window = crate::render::Window::new(position, depth, size);
        
        let widget = StaticTextWidget::<C> {
            children: vec![],
            name,
            parent_index: None,
            size_and_position,
            render_text,
            __phantom: std::marker::PhantomData,
        };
        
        Ok((widget, window))
    }
}

/// Implementation of the methods for StaticTextWidget
impl<C> Widget<C> for StaticTextWidget<C> {
    /// Returns the widget's name as an identifier. The rendering backend relies
    /// on `String` names instead of widgets.
    fn get_window_ref(&self) -> String {
        self.name.clone()
    }
    
    // for handling updates (a static widget would just have this empty)
    /// Handles event updates (no-op for static widgets as they don't respond to events)
    fn update_with_events(&mut self, _data: &mut C, _app: &mut crate::App<C>, _scene: &mut Scene<C>) {
        // the static widget doesn't need to change
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
        if window.is_empty() {  // it'll only be empty if nothing has been assigned; once something is assigned that text is final
            window.try_update_lines(self.render_text.clone())
        } else {  false  }
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
