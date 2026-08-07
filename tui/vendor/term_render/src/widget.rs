#![allow(dead_code)]  // to avoid redundant warnings as this is a library module

// handles widgets and all between
use crate::{render as term_render, render, App};

// I don't like the all unsafe, but I don't see an easy way around it without
// complicating the API and usage significantly.
// The Scene calls the event update, but needs to pass itself and a reference to some of its data.
// This is pretty much just a wrapper for a pointer to the actual widget, hiding the nasty unsafe stuff.
/// A struct that manages event queuing for a widget within a scene.
///
/// `WidgetEventQueuer` holds a raw pointer to a widget, allowing event handling
/// without violating Rust's borrowing rules. The lifetime safety is ensured by
/// retaining ownership within the scene, and passing ownership of the
/// `WidgetEventQueuer` instead of the widget itself. This design prevents
/// multiple mutable references and ensures that the scene outlives any
/// `WidgetEventQueuer` instances.
///
/// # Type Parameters
/// - `C`: The user-defined data type associated with the main application.
///
/// # Safety
/// The raw pointer to the widget (`owner`) is safe as long as the scene is not
/// dropped while a `WidgetEventQueuer` exists. Dropping the queuer removes
/// references to the widget pointer.
///
/// # Fields
/// - `owner`: Raw pointer to the widget implementing the `Widget` trait.
/// - `_phantom`: Marker to associate the context type `C` with the struct.
pub struct WidgetEventQueuer<C> {
    // The following fields should cover the returned data fields of the Widget trait.
    // They are raw pointers to avoid multiple references issues (even though multiple
    // won't be mutated at the same time).

    // This should be safe lifetime wise as, the ownership is retained within the scene,
    // and rather the ownership of a new WidgetEventQueuer is passed around.
    // Therefore, the lifetime is still attached to the scene, and not the WidgetEventQueuer itself.
    // The user shouldn't easily be able to drop the scene while holding onto an WidgetEventQueuer.
    // Dropping the WidgetEventQueuer will remove references to the pointers.
    
    /// Raw pointer to the widget implementing the `Widget` trait.
    owner: *mut dyn Widget<C>,

    _phantom: std::marker::PhantomData<C>,
}

impl<C> WidgetEventQueuer<C> {
    /// Creates a new `WidgetEventQueuer` instance with the specified owner.
    ///
    /// # Arguments
    ///
    /// * `owner` - A raw pointer to a type that implements the `Widget<C>` trait. This represents the owner of the event queuer.
    ///
    /// # Returns
    ///
    /// A new instance of `WidgetEventQueuer` associated with the given owner.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the `owner` pointer remains valid for the lifetime of the `WidgetEventQueuer`.
    pub fn new(owner: *mut dyn Widget<C>) -> Self {
        WidgetEventQueuer {
            owner,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<C> Widget<C> for WidgetEventQueuer<C> {
    /// Returns a unique identifier string for the widget's associated window.
    /// This connects the widget to its rendering surface in the terminal.
    fn get_window_ref(&self) -> String {
        unsafe {  (*self.owner).get_window_ref()  }
    }
    
    /// Processes input events and updates widget state accordingly.
    /// Static widgets may leave this empty, while interactive widgets should respond to events.
    fn update_with_events(&mut self, data: &mut C, app: &mut App<C>, scene: &mut Scene<C>) {
        unsafe {  (*self.owner).update_with_events(data, app, scene);  }
    }
    
    /// Updates the widget's visual representation based on current state.
    /// Called automatically during render passes to refresh the terminal display.
    /// Static widgets may leave this empty, while interactive widgets should respond to events.
    /// Returns true if the widget's content changed and needs re-rendering (mainly to indicate
    /// the need for re-rendering the parents).
    fn update_render(&mut self, window: &mut term_render::Window, area: &term_render::Rect) -> bool {
        unsafe {  (*self.owner).update_render(window, area)  }
    }
    
    /// Returns indices of child widgets for scene graph traversal.
    fn get_children_indexes(&self) -> Vec<usize> { unsafe { (*self.owner).get_children_indexes() } }

    /// Adds a child widget index to maintain parent-child relationships.
    fn add_child_index(&mut self, index: usize) {
        unsafe {  (*self.owner).add_child_index(index);  }
    }

    /// Removes a child widget index from this widget.
    fn remove_child_index(&mut self, index: usize) {
        unsafe {  (*self.owner).remove_child_index(index);  }
    }

    /// Clears all child widget indices from this widget.
    fn clear_children_indexes(&mut self) {
        unsafe {  (*self.owner).clear_children_indexes();  }
    }
    
    /// Returns the parent widget index if one exists, otherwise returns `None`.
    fn get_parent_index(&self) -> Option<usize> {
        unsafe {  (*self.owner).get_parent_index()  }
    }
    
    /// Sets or clears the parent widget index for hierarchy management.
    fn set_parent_index(&mut self, index: Option<usize>) {
        unsafe {  (*self.owner).set_parent_index(index);  }
    }
    
    /// Checks if a given position collides with the widget's area.
    fn is_collided(&self, position: (u16, u16)) -> bool {
        unsafe {  (*self.owner).is_collided(position)  }
    }
}

/// Core trait defining the interface for all UI widgets in the scene graph.
/// Provides methods for event handling, rendering, and managing parent-child relationships.
/// Implementors must provide a window reference and handle updates.
/// The trait represents an abstracted layer above the base Window struct.
/// Type parameter T allows passing application-specific data to widgets during updates.
pub trait Widget<T> {
    /// Returns a unique identifier string for the widget's associated window.
    /// This connects the widget to its rendering surface in the terminal.
    fn get_window_ref(&self) -> String;
    
    /// Processes input events and updates widget state accordingly.
    /// Static widgets may leave this empty, while interactive widgets should respond to events.
    fn update_with_events(&mut self, data: &mut T, app: &mut App<T>, scene: &mut Scene<T>);
    
    /// Updates the widget's visual representation based on current state.
    /// Called automatically during render passes to refresh the terminal display.
    /// Static widgets may leave this empty, while interactive widgets should respond to events.
    /// Returns true if the widget's content changed and needs re-rendering (mainly to indicate
    /// the need for re-rendering the parents).
    fn update_render(&mut self, window: &mut term_render::Window, area: &term_render::Rect) -> bool;
    
    /// Returns indices of child widgets for scene graph traversal.
    fn get_children_indexes(&self) -> Vec<usize>;

    /// Adds a child widget index to maintain parent-child relationships.
    fn add_child_index(&mut self, index: usize);

    /// Removes a child widget index from this widget.
    fn remove_child_index(&mut self, index: usize);

    /// Clears all child widget indices from this widget.
    fn clear_children_indexes(&mut self);
    
    /// Returns the parent widget index if one exists, otherwise returns `None`.
    fn get_parent_index(&self) -> Option<usize>;
    
    /// Sets or clears the parent widget index for hierarchy management.
    fn set_parent_index(&mut self, index: Option<usize>);
    
    /// Checks if a given position collides with the widget's area.
    fn is_collided(&self, position: (u16, u16)) -> bool;
}

/// Error type for widget operations, containing descriptive error messages.
/// Used throughout the widget system for consistent error handling.
#[derive(Debug)]
pub struct WidgetErr {
    details: String,
}

impl std::fmt::Display for WidgetErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WidgetErr: {}", self.details)
    }
}

impl WidgetErr {
    /// Creates a new `WidgetErr` with the given details. Currently,
    /// this is a very simple struct format, so details will already need
    /// to be coalesced into a single string.
    pub fn new(details: &str) -> Self {
        WidgetErr { details: details.to_string() }
    }
}

/// A specialized vector that maintains stable indices after item removal.
/// Uses a free-list of reserved positions to allow O(1) insertion/removal
/// while preserving indices of existing elements - crucial for UI scene graphs.
struct PositionReservedVector<C, T: Widget<C> + ?Sized> {
    /// The underlying vector storing widgets or None for reserved positions.
    pub vector: Vec<Option<Box<T>>>,
    /// List of indices that have been removed and can be reused.
    reserved_positions: Vec<usize>,
    taken: Option<(String, usize)>,
    event_queuer: Option<WidgetEventQueuer<C>>,
    _phantom: std::marker::PhantomData<C>,
}

impl <C, T: ?Sized + Widget<C>> PositionReservedVector<C, T> {
    /// Helper method to get a raw pointer for trait objects
    /// This uses unsafe code but is necessary for the event queuer system
    fn get_widget_raw_ptr(widget: &Box<T>) -> *mut dyn Widget<C> {
        unsafe {
            // We use std::ptr::addr_of! to get the address without creating an intermediate reference
            // This works because Box guarantees the pointer is valid
            let box_ptr = widget.as_ref() as *const T;
            std::mem::transmute_copy::<*const T, *mut dyn Widget<C>>(&box_ptr)
        }
    }

    /// Sets the internal event queuer to point to the widget at the given index.
    /// If the index is invalid or the position is reserved, clears the event queuer.
    /// This allows safe mutable access to the widget for event handling.
    pub fn set_mut_widget_ptr(&mut self, index: usize) {
        if index >= self.vector.len() {
            return;
        }
        if let Some(widget) = &self.vector[index] {
            // We know that T is dyn Widget<C> in practice
            // Use a helper function to get the raw pointer safely
            let raw_ptr = Self::get_widget_raw_ptr(widget);
            let queue = WidgetEventQueuer::new(raw_ptr);
            self.event_queuer = Some(queue);
        } else {
            self.event_queuer = None;
        }
    }

    /// Takes ownership of the item at the given index, replacing it with `None`.
    /// Acts similar to using `Option::take()`.
    pub fn take(&mut self, index: usize) -> Option<Box<T>> {
        if index >= self.vector.len() {
            return None;
        }
        self.taken = Some((self.vector[index].as_ref()?.get_window_ref(), index));
        let widget = self.vector[index].take();
        // creating an event queuer to allow for synchronization of called methods to the widget trait
        if let Some(widget) = &widget {
            // We know that T is dyn Widget<C> in practice
            // Use a helper function to get the raw pointer safely
            let raw_ptr = Self::get_widget_raw_ptr(widget);
            let queue = WidgetEventQueuer::new(raw_ptr);
            self.event_queuer = Some(queue);
        }
        widget
    }

    /// Replaces the item at the given index with a new item or `None` (undoes the `take` call).
    pub fn replace(&mut self, index: usize, item: Option<Box<T>>) {
        self.taken = None;
        self.event_queuer = None;
        if index < self.vector.len() {
            self.vector[index] = item;
        }
    }
    
    /// Removes an item at the given index, replacing it with `None`, leaving the indices intact.
    /// Marks the position as reserved for future reuse.
    /// Returns the removed item or an error if index is invalid.
    pub fn remove(&mut self, index: usize) -> Result<Box<T>, WidgetErr> {
        if index >= self.vector.len() {
            return Err(WidgetErr::new("Index out of bounds"));
        }
        if self.reserved_positions.contains(&index) {
            return Err(WidgetErr::new("Index is reserved and cannot be removed"));
        }
        
        let item = self.vector.remove(index);
        self.vector.insert(index, None);  // replace with a default value to maintain indices
        self.reserved_positions.push(index);
        
        Ok(match item {
            Some(i) => i,
            None => return Err(WidgetErr::new("Invalid widget index - 1")),
        })
    }
    
    /// Pushes an item into the vector, reusing reserved positions if available.
    /// Returns the index where the item was placed.
    pub fn push(&mut self, item: Box<T>) -> usize {
        if let Some(reserved_index) = self.reserved_positions.pop() {
            self.vector[reserved_index] = Some(item);
            reserved_index
        } else {
            self.vector.push(Some(item));
            self.vector.len() - 1
        }
    }
    
    /// Returns the number of items in the vector (including reserved positions).
    pub fn len(&self) -> usize {
        self.vector.len()
    }

    /// Returns the number of actual items (excluding reserved positions).
    pub fn adjusted_len(&self) -> usize {
        self.vector.len() - self.reserved_positions.len()
    }
    
    /// Returns a reference to the item at the given index, if it exists.
    /// If the index is out of bounds *or* reserved, returns None.
    pub fn index(&self, index: usize) -> Option<&Box<T>> {
        if index >= self.vector.len() {
            return None;
        }
        self.vector[index].as_ref()
    }
    
    /// Returns a mutable reference to the item at the given index, if it exists.
    /// If the index is out of bounds *or* reserved, returns None.
    pub fn index_mut(&mut self, index: usize) -> Option<&mut Box<T>> {
        if index >= self.vector.len() {
            return None;
        }
        self.vector[index].as_mut()
    }
}

/// Manages a collection of widgets and their hierarchical relationships.
/// Handles rendering coordination, event propagation, and widget lifecycle.
pub struct Scene<C> {
    /// All widgets in the scene
    widgets: PositionReservedVector<C, dyn Widget<C>>,
}

impl<C> Scene<C> {
    /// Creates a new empty scene with no widgets.
    pub fn new() -> Self {
        Scene {
            widgets: PositionReservedVector {
                vector: Vec::new(),
                reserved_positions: Vec::new(),
                taken: None,
                event_queuer: None,
                _phantom: std::marker::PhantomData,
            },
        }
    }

    /// Finds the index of a widget by its window reference name.
    /// Returns `Some(index)` if found, otherwise returns `None`.
    pub fn get_widget_index(&self, widget_name: String) -> Option<usize> {
        if widget_name == self.widgets.taken.as_ref().unwrap_or(&(String::from(""), 0)).0 {
            return Some(self.widgets.taken.as_ref().unwrap().1);
        }
        for i in 0..self.widgets.len() {
            if let Some(widget) = self.widgets.index(i) {
                if widget.get_window_ref() == widget_name {
                    return Some(i);
                }
            }
        } None
    }
    
    /// Returns a reference to the widget at the given index.
    /// Returns an error if the index is out of bounds.
    pub fn widget_as_ref(&self, index: usize) -> Result<&Box<dyn Widget<C>>, WidgetErr> {
        self.widgets.index(index).ok_or(WidgetErr::new("Index out of bounds"))
    }
    
    /// Returns a mutable reference to the widget at the given index.
    /// Returns an error if the index is out of bounds.
    pub fn widget_as_mut(&mut self, index: usize) -> Result<&mut Box<dyn Widget<C>>, WidgetErr> {
        self.widgets.index_mut(index).ok_or(WidgetErr::new("Index out of bounds"))
    }
    
    // whenever a widget is updated, all its parents need to be updated as well
    /// Adds a widget to the scene and registers its window with the renderer.
    /// Establishes parent-child relationships and handles root node assignment.
    /// Returns the index where the widget was placed.
    pub fn add_widget(&mut self, widget: Box<dyn Widget<C>>, window: term_render::Window, app: &mut term_render::App) -> Result<usize, WidgetErr> {
        app.add_window(window, widget.get_window_ref(), vec![]);
        
        //let index = self.widgets.len();
        let parent_index = widget.get_parent_index();
        let index = self.widgets.push(widget);
        
        // adding the optional parent-child relationship (only the root node can be parentless)
        if let Some(parent_index) = &parent_index {
            // Fix the syntax - use proper error handling
            match self.widgets.index_mut(*parent_index) {
                Some(parent_widget) => parent_widget.add_child_index(index),
                None => return Err(WidgetErr::new("Invalid widget index - 2")),
            }
        }
        
        Ok(index)
    }
    
    /// A wrapper around `remove_widget` that takes a widget reference name instead of an index.
    /// This is to reduce the boilerplate of getting the index first often through a chain of functions.
    pub fn remove_widget_ref(&mut self, r#ref: String, app: &mut term_render::App) -> Result<(), WidgetErr> {
        let index = self.get_widget_index(r#ref).ok_or(WidgetErr::new("Widget not found"))?;
        self.remove_widget(index, app)
    }
    
    /// Removes a widget and all its children recursively.
    /// Handles window cleanup, parent relationship updates, and resource management.
    /// If a parent is removed, all its children are also removed.
    /// Returns an error if the index is out of bounds or reserved (look at
    /// `PositionReservedVector::reserved_positions` for more information on reservations).
    pub fn remove_widget(&mut self, index: usize, app: &mut term_render::App) -> Result<(), WidgetErr> {
        // checking if it's out of range, or a reserved index
        if index >= self.widgets.len() || self.widgets.index(index).is_none() {
            return Err(WidgetErr::new("Index out of bounds"));
        }
        
        app.remove_window(self.widgets.index(index).unwrap().get_window_ref()).unwrap();
        
        // updating the parents windows
        if self.widgets.index(index).unwrap().get_parent_index().is_some() {
            self.update_parents(index, app)?;
        }
        
        // remove from parent's children list
        if let Some(parent_index) = match self.widgets.index(index){
            Some(w) => w,
            None => return Err(WidgetErr::new("Invalid widget index - 10")),
        }.get_parent_index() {
            let parent_widget = match self.widgets.index_mut(parent_index) {
                Some(w) => w,
                None => return Err(WidgetErr::new("Invalid widget index - 4")),
            };
            let child_index_location = parent_widget.get_children_indexes().iter().position(|&i| i == index).ok_or(WidgetErr::new("Child index not found in parent"))?;
            parent_widget.remove_child_index(child_index_location);
        }
        
        // remove all children recursively
        let children = match self.widgets.index(index){
            Some(w) => w,
            None => return Err(WidgetErr::new("Invalid widget index - 5")),
        }.get_children_indexes();
        for &child_index in &children {
            match self.widgets.index_mut(child_index) {
                Some(child_widget) => child_widget.set_parent_index(None),
                None => return Err(WidgetErr::new("Invalid child widget index - 82")),
            }
            self.remove_widget(child_index, app)?;
        }
        
        // finally, remove the widget itself
        self.widgets.remove(index)?;
        
        Ok(())
    }
    
    /// Updates all widgets in the scene with current events and refreshes their rendering.
    /// Processes events first, then updates visual representation for each widget.
    /// If a widget's content changes, its parents are also updated to reflect the change.
    /// This ensures the entire scene graph remains consistent and up-to-date.
    pub fn update_all_widgets(&mut self, app_main: &mut crate::App<C>, data: &mut C) -> Result<(), WidgetErr> {
        for i in 0..self.widgets.len() {  // the if let skips reserved indices
            if self.widgets.index(i).is_none() {  continue;  }
            
            self.widgets.set_mut_widget_ptr(i);
            let mut widget = match self.widgets.event_queuer.take() {
                None => return Err(WidgetErr::new("Failed to gather the event queuer")),
                Some(ptr) => ptr,
            };
            //self.widgets.replace(i, Some(widget_safe));  // put the widget back
            
            widget.update_with_events(data, app_main, self);
            let window = widget.get_window_ref();
            if widget.update_render(app_main.renderer.write().get_window_reference_mut(window), &*app_main.area.read()) && widget.get_parent_index().is_some() {
                // if the widget changed, update all its children
                self.update_children(i, &mut *app_main.renderer.write())?;
            }
        } Ok(())
    }

    /// Recursively updates all child widgets of the widget at the given index.
    /// Ensures visual consistency when parent widgets change.
    fn update_children(&mut self, index: usize, app: &mut term_render::App) -> Result<(), WidgetErr> {
        let children = match self.widgets.index(index){
            Some(w) => w,
            None => return Err(WidgetErr::new("Invalid widget index - 42")),
        }.get_children_indexes();
        for &child_index in &children {
            let widget = match self.widgets.index_mut(child_index) {
                Some(w) => w,
                None => return Err(WidgetErr::new("Invalid widget index - 13")),
            };
            app.get_window_reference_mut(widget.get_window_ref()).update_all();
            self.update_children(child_index, app)?;
        } Ok(())
    }
    
    /// Forces a complete update of all widget windows regardless of dirty state.
    /// Useful when the terminal is resized or major layout changes occur (although,
    /// in most cases, it shouldn't be necessary, and should automatically be handled).
    pub fn force_update_all_widgets(&mut self, app: &mut render::App) {
        for i in 0..self.widgets.len() {
            if let Some(widget) = self.widgets.index_mut(i) {
                //widget.update_render();  // this should already have been done
                app.get_window_reference_mut(widget.get_window_ref()).update_all();
            }
        }
    }
    
    /// Updates a specific widget and its rendering.
    /// Also triggers updates to parent widgets to maintain consistency if the window is updated.
    pub fn update_widget(&mut self, index: usize, app_main: &mut crate::App<C>, area: &term_render::Rect, data: &mut C) -> Result<(), WidgetErr> {
        if index >= self.widgets.len() || self.widgets.index(index).is_none() {
            return Err(WidgetErr::new("Index out of bounds"));
        }
        
        let mut widget = match self.widgets.take(index) {
            Some(w) => w,
            None => return Err(WidgetErr::new("Invalid widget index - 6")),
        };
        widget.update_with_events(data, app_main, self);
        self.widgets.replace(index, Some(widget));  // put the widget back
        let widget =match self.widgets.index_mut(index) {
            Some(w) => w,
            None => return Err(WidgetErr::new("Invalid widget index - 12")),
        };
        let renderer = &mut *app_main.renderer.write();
        let window = renderer.get_window_reference_mut(widget.get_window_ref());
        if widget.update_render(window, area) && widget.get_parent_index().is_some() {
            self.update_parents(index, &mut *app_main.renderer.write())?;
        }
        
        Ok(())
    }
    
    /// Updates only the rendering of a specific widget without processing events.
    /// Useful for visual-only changes that don't affect widget state.
    pub fn update_widget_renderer(&mut self, index: usize, app: &mut term_render::App, area: &term_render::Rect) -> Result<(), WidgetErr> {
        let widget = match self.widgets.index_mut(index) {
            Some(w) => w,
            None => return Err(WidgetErr::new("Invalid widget index - 7")),
        };
        let window = app.get_window_reference_mut(widget.get_window_ref());
        if widget.update_render(window, area) && widget.get_parent_index().is_some() {
            self.update_parents(index, app)?;
        }
        Ok(())
    }
    
    /// Recursively updates all parent widgets of the widget at the given index.
    /// Ensures visual consistency when child widgets change.
    fn update_parents(&mut self, index: usize, app: &mut term_render::App) -> Result<(), WidgetErr> {
        if let Some(parent_index) = match self.widgets.index(index){
            Some(w) => w,
            None => return Err(WidgetErr::new("Invalid widget index - 8")),
        }.get_parent_index() {
            let widget = match self.widgets.index_mut(parent_index) {
                Some(w) => w,
                None => return Err(WidgetErr::new("Invalid widget index - 9")),
            };
            app.get_window_reference_mut(widget.get_window_ref()).update_all();
            self.update_parents(parent_index, app)?;
        } Ok(())
    }
    
    /// Checks if a click at the given position is blocked by any other widgets in the scene.
    /// Returns `Some(true)` if blocked, `Some(false)` if not blocked, or `None` if the index is invalid.
    /// This is useful for determining if a click event should be processed by the widget or ignored due to overlap.
    /// This is different from `is_click_blocked` which only checks child widgets.
    pub fn is_click_blocked_all(&self, index: usize, position: (u16, u16), app: &App<C>) -> Option<bool> {
        let base_depth = app.renderer.read().get_window_reference(match self.widgets.index(index) {
            Some(w) => w,
            None => return None,
        }.get_window_ref()).depth;
        for i in 0..self.widgets.len() {
            if i == index {  continue;  }
            if let Some(widget) = self.widgets.index(i) {
                if widget.is_collided(position) &&
                    base_depth < app.renderer.read().get_window_reference(widget.get_window_ref()).depth
                {
                    return Some(true)
                }
            }
        } Some(false)  // None means bad index, false means it's not blocked
    }
    
    /// Checks if a click at the given position is blocked by any child widgets of the widget at the given index.
    /// Returns `Some(true)` if blocked, `Some(false)` if not blocked, or `None` if the index is invalid.
    /// This is useful for determining if a click event should be processed by the widget or ignored due to overlap.
    pub fn is_click_blocked(&self, index: usize, position: (u16, u16)) -> Option<bool> {
        let children = &self.widgets.index(index)?.get_children_indexes();
        for child in children {
            let widget = self.widgets.index(*child)?;
            if widget.is_collided(position) {
                return Some(true);
            }
            // recursively check children
            if let Some(blocked) = self.is_click_blocked(*child, position) {
                if blocked {
                    return Some(true);
                }
            }
        } Some(false)  // None means bad index, false means it's not blocked
    }
}

