#![allow(dead_code)]

use crate::widget_impls::*;
use crate::widget::*;

/// Builder for creating DynamicWidget instances with a fluent interface.
/// Maintains configuration state until build() is called to create the actual widget.
/// `DynamicWidgetBuilder` is an example of an implementation of `WidgetBuilder`, where
/// the struct doesn't implement `Widget`.
/// Type C represents the application data type, which can be any type defined by the user.
pub struct DynamicWidgetBuilder<C> {
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
    pub render_function: Option<Box<dyn Fn((u16, u16), (u16, u16)) -> Option<Vec<crate::render::Span>>>>,
    /// The update handler function for the widget, if any. This function is called during event updates.
    /// The closure receives references to the widget itself, the event parser, and mutable application data.
    /// In responce, the closure can react to events and modify the widget's state as needed.
    update_handler: Option<Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>>,
    /// The index of the parent widget in the scene graph, if any.
    parent: Option<usize>,

    __phantom: std::marker::PhantomData<C>,
}

/// Implementations for the methods in `WidgetBuilder`.
impl<C: 'static> WidgetBuilder<C> for DynamicWidgetBuilder<C> {
    /// Constructs a `DynamicWidget`, an implementor of `Widget`, given the parameters.
    /// Validates that size and position are non-zero before creating the widget.
    /// The method takes in a reference to the terminal's current area/size.
    /// # Example:
    /// ```
    /// use term_render::widget_impls::{DynamicWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    /// let (widget, window) = DynamicWidgetBuilder::<AppData>::builder(String::new())
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
        Ok((Box::new(DynamicWidget::<C> {
            children: vec![],
            name: self.name,
            parent_index: self.parent,
            size_and_position: self.size_and_position,
            render_function: self.render_function,
            update_handler: self.update_handler,
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
    type RendererType = Box<dyn Fn((u16, u16), (u16, u16)) -> Option<Vec<crate::render::Span>>>;
    /// Sets the rendering closure that generates content for the widget.
    /// The closure receives size and position parameters and returns an optional vector of type `Span`.
    /// By default, there is no renderer, leaving the widget empty (apart from stylization like a border or title).
    /// The closure is a boxed closure that takes in `(size: (u16, u16), position: (u16, u16))`. This closure
    /// can capture local context to allow for easier dynamic variations between widgets with minimal boilerplate.
    /// #Example:
    /// ```
    /// use term_render::widget_impls::{DynamicWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    ///
    /// // the closure can capture local variables to reduce boilerplate
    /// let closure = Box::new(move |size, position| {
    ///     None  // this will leave the widget un-updated
    /// });
    /// let (widget, window) = DynamicWidgetBuilder::<AppData>::builder(String::new())
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
    /// use term_render::widget_impls::{DynamicWidgetBuilder, WidgetBuilder};
    /// let builder = DynamicWidgetBuilder::<AppData>::builder(String::from("Widget Name"));
    /// ```
    fn builder(name: String) -> Self {
        Self {
            name,
            depth: None,
            size_and_position: SizeAndPosition::default(),
            render_function: None,
            border: false,
            title: None,
            update_handler: None,
            parent: None,
            __phantom: std::marker::PhantomData,
        }
    }
    
    /// Sets the SizeAndPosition configuration directly.
    fn with_sap(mut self, sap: SizeAndPosition) -> Self {
        self.size_and_position = sap;
        self
    }

    /// Sets the widget's update handler closure. This closure is called during event updates.
    /// The closure receives references to the widget itself, the event parser, and mutable application data.
    /// By default, there is no update handler, meaning the widget won't respond to events.
    type FunctionType = Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>;
    fn with_update_handler(mut self, handler: Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>) -> Self {
        self.update_handler = Some(handler);
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
    /// use term_render::widget_impls::{DynamicWidgetBuilder, WidgetBuilder};
    /// use term_render::render::Rect;
    /// let mut app = term_render::App::new().unwrap();
    /// let mut scene = term_render::widget::Scene::new();
    /// let widget_index = DynamicWidgetBuilder::<AppData>::builder(String::from("My Widget"))
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

/// A widget that renders dynamic content using a provided closure (i.e.
/// a button or interactable component).
/// Suitable for content that changes frequently or in response to events.
/// However, other widget implementations may be more appropriate for complex interactions
/// or multi-button arrangements within a single widget.
/// `DynamicWidgetBuilder` is the associated builder for creating instances of this widget.
pub struct DynamicWidget<C> {
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
    pub render_function: Option<Box<dyn Fn((u16, u16), (u16, u16)) -> Option<Vec<crate::render::Span>>>>,

    /// Optional closure that handles updates to the widget's state.
    pub update_handler: Option<Box<dyn Fn(&mut dyn Widget<C>, &mut C, &mut crate::App<C>, &mut Scene<C>)>>,

    __phantom: std::marker::PhantomData<C>,
}

impl<C> DynamicWidget<C> {
    /// Creates a new DynamicWidget and its associated window.
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
               render_function: Option<Box<dyn Fn((u16, u16), (u16, u16)) -> Option<Vec<crate::render::Span>>>>,
               depth: u16,
               display_area: &crate::render::Rect,
    ) -> Result<(DynamicWidget<C>, crate::render::Window), WidgetErr> {
        let (size, position) = size_and_position.get_size_and_position(display_area);
        if size.0 == 0 || size.1 == 0 || position.0 == 0 || position.1 == 0 {
            return Err(WidgetErr::new("Size or position cannot be zero"))
        }
        let window = crate::render::Window::new(position, depth, size);
        
        let widget = DynamicWidget::<C> {
            children: vec![],
            name,
            parent_index: None,
            size_and_position,
            render_function,
            update_handler: None,
            __phantom: std::marker::PhantomData,
        };
        
        Ok((widget, window))
    }
}

/// Implementation of the methods for DynamicWidget
impl<C> Widget<C> for DynamicWidget<C> {
    /// Returns the widget's name as an identifier. The rendering backend relies
    /// on `String` names instead of widgets.
    fn get_window_ref(&self) -> String {
        self.name.clone()
    }
    
    /// Handles event updates by invoking the user-provided update handler closure, if any.
    /// The closure receives references to the widget itself, the event parser, and mutable application data.
    /// If no update handler is set, this method performs no action.
    fn update_with_events(&mut self, data: &mut C, app: &mut crate::App<C>, scene: &mut Scene<C>) {
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
            if let Some(render) = render_function(size, position) {
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


