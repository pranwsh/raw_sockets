#![allow(dead_code, unused_imports)]

// Making all of them publicly accessible from this module to prevent importing 20 modules and preventing me from writing it all in one file
pub use crate::widget_static_text::*;
pub use crate::widget_dynamic::*;
pub use crate::widget_typing::*;
pub use crate::widget_static::*;
pub use crate::widget_button::*;
use crate::widget::*;

/// A builder trait for constructing widgets with a fluent interface.
/// Provides methods to configure widget properties like position, size, borders, and rendering behavior.
/// Implementors should provide a `build` method that creates the widget and its associated window.
/// Implementors often *won't* implement `Widget`, but the returned type from `build` will.
pub trait WidgetBuilder<C> {
    /// Constructs the widget and its associated window based on the current configuration.
    /// It's advised, but not required, the implementor doesn't implement `Widget`.
    /// The returned type does have to implement `Widget`.
    fn build(self, display_area: &crate::render::Rect) -> Result<(Box<dyn Widget<C>>, crate::render::Window), WidgetBuilderError>;
    /// Sets the widget's position
    fn with_position(self, position: (u16, u16)) -> Self;
    /// Sets the widget's size
    fn with_size(self, size: (u16, u16)) -> Self;
    /// Sets the widget's dynamic position
    fn with_dynamic_position(self, position_offset: (i16, i16), position_area_percent: (f32, f32)) -> Self;
    /// Sets the widget's dynamic size
    fn with_dynamic_size(self, size_offset: (i16, i16), size_area_percent: (f32, f32)) -> Self;
    /// Sets if the widget has a border or not
    fn with_border(self, border: bool) -> Self;
    /// Sets the widget's title (displayed in border if enabled; invisible otherwise)
    fn with_title(self, title: String) -> Self;
    /// Sets the widget's depth (z-index)
    fn with_depth(self, depth: u16) -> Self;
    /// The type representing the renderer closure.
    type RendererType;
    /// Sets the widget's custom renderer closure
    fn with_renderer(self, renderer: Self::RendererType) -> Self;
    /// Creates a new builder instance with the provided unique name identifier.
    /// It's recommended that the identifying name is as inputted, and not
    /// modified, to avoid conflicts if the user manually attempts to access the widget.
    fn builder(name: String) -> Self;
    /// Sets the widget's SizeAndPosition configuration directly.
    fn with_sap(self, sap: SizeAndPosition) -> Self;
    /// The type for the update handler closure.
    type FunctionType;
    /// Sets the widget's update handler closure. This closure is called during event updates.
    fn with_update_handler(self, _handler: Self::FunctionType) -> Self;
    /// Sets the widget's parent by its index in the scene, or `None` for no parent (root level).
    fn with_parent(self, parent: Option<usize>) -> Self;
    /// Builds and adds the widget to the scene, removing the boilerplate of calling `build` and then adding it to the scene.
    fn add_to_scene(self, app: &mut crate::App<C>, scene: &mut Scene<C>) -> Result<usize, WidgetErr>;
}

/// Represents a widget's size and position configuration, supporting both static and dynamic layouts.
/// Dynamic layouts are calculated relative to the terminal area with optional fixed offsets.
/// Static layouts are solely based on the provided constant, and as such won't change if the terminal
/// is resized.
#[derive(Default)]
pub struct SizeAndPosition {
    /// Fixed size adjustment (width, height) in characters
    pub size_offset: (i16, i16),
    /// Fixed position adjustment (x, y) in character coordinates
    pub position_offset: (i16, i16),
    
    /// Size as percentage of terminal area (width%, height%) in the range [0, 1]
    /// The final size is calculated as:
    /// `final_size = (terminal_area * size_area_percent) + size_offset`
    pub size_area_percent: (f32, f32),  // percentage of the terminal area (0.5 is the center)
    /// Position as percentage of terminal area (x%, y%) in the range [0, 1]
    /// The final position is calculated as:
    /// `final_position = (terminal_area * position_area_percent) + position_offset`
    pub position_area_percent: (f32, f32),  // percentage of the terminal area (0.5 is the center)
    
    /// Caches the last calculated size and position to avoid redundant calculations.
    pub last_size_pos: ((u16, u16), (u16, u16)), // caching the last calculated size and position
}

impl SizeAndPosition {
    /// Creates a SizeAndPosition with fixed size and position that doesn't respond to terminal resizing.
    /// # Arguments
    /// * `size` - Fixed dimensions (width, height) in characters
    /// * `position` - Fixed position (x, y) in character coordinates
    ///
    /// *- The relative size and position to the terminal's area are set to 0.*
    pub fn new_static(size: (u16, u16), position: (u16, u16)) -> SizeAndPosition {
        SizeAndPosition {
            size_offset: (size.0 as i16, size.1 as i16),
            position_offset: (position.0 as i16, position.1 as i16),
            size_area_percent: (0.0, 0.0),
            position_area_percent: (0.0, 0.0),
            last_size_pos: ((0, 0), (0, 0)),
        }
    }
    
    /// Creates a SizeAndPosition that dynamically adjusts based on terminal size.
    /// # Arguments
    /// * `size_offset` - Fixed size adjustment (width, height) in characters
    /// * `position_offset` - Fixed position adjustment (x, y) in character coordinates
    /// * `size_area_percent` - Size as percentage of terminal area (width%, height%) in the range [0, 1]
    /// * `position_area_percent` - Position as percentage of terminal area (x%, y%) in the range [0, 1]
    pub fn new_dynamic(size_offset: (i16, i16), position_offset: (i16, i16), size_area_percent: (f32, f32), position_area_percent: (f32, f32)) -> SizeAndPosition {
        SizeAndPosition {
            size_offset,
            position_offset,
            size_area_percent,
            position_area_percent,
            last_size_pos: ((0, 0), (0, 0)),
        }
    }
    
    /// Calculates the actual size and position based on the current terminal area.
    /// # Arguments
    /// * `area` - The current terminal display area
    /// # Returns
    /// Tuple containing ((width, height), (x, y)) coordinates
    ///  - *Note: static layout configurations will always return the values regardless of the inputted area.*
    pub fn get_size_and_position(&mut self, area: &crate::render::Rect) -> ((u16, u16), (u16, u16)) {
        let width = (((area.width as f32) * self.size_area_percent.0) as i16 + self.size_offset.0) as u16;
        let height = (((area.height as f32) * self.size_area_percent.1) as i16 + self.size_offset.1) as u16;
        
        let x = (((area.width as f32) * self.position_area_percent.0) as i16 + self.position_offset.0) as u16;
        let y = (((area.height as f32) * self.position_area_percent.1) as i16 + self.position_offset.1) as u16;
        
        self.last_size_pos = ((width, height), (x, y));
        
        ((width, height), (x, y))
    }
    
    pub fn get_last(&self) -> ((u16, u16), (u16, u16)) {
        self.last_size_pos
    }
}

/// Error type for widget building operations, containing details about what went wrong.
#[derive(Debug)]
pub struct WidgetBuilderError {
    pub details: String,
}

impl std::fmt::Display for WidgetBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WidgetBuilderError: {}", self.details)
    }
}
