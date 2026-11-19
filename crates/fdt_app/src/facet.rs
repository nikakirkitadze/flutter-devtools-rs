//! A cached view of one part of another view.
//!
//! GPUI re-renders a view on every frame its ancestors were notified, so a toolbar built
//! inline by a list view is rebuilt on every scroll event. A `Facet` is a view of its own that
//! builds one part of its owner, and embedded with `.cached(..)` it is replayed from the
//! previous frame until it is notified.
//!
//! The owner does not have to tell the facet anything. The facet observes the owner and, when
//! the owner is notified, compares a key of everything the part reads with the key it was last
//! built from. A notification that changes nothing the part shows (a scroll) costs one key
//! comparison and no render.

use gpui_kit::{
    AnyElement, App, Context, Entity, IntoElement, Render, Subscription, WeakEntity, Window, div,
};

/// Builds the part from its owner.
pub type Build<V> = fn(&mut V, &mut Window, &mut Context<V>) -> AnyElement;

/// Everything the part reads from its owner, as text. Two equal keys mean the same pixels.
pub type Key<V> = fn(&V, &App) -> String;

pub struct Facet<V: 'static> {
    owner: WeakEntity<V>,
    build: Build<V>,
    key: Key<V>,
    /// The key the part was last built from.
    built_from: String,
    subs: Vec<Subscription>,
}

impl<V: 'static> Facet<V> {
    /// A facet of `owner`. Create it inside the owner's constructor, or later.
    pub fn new(owner: &Entity<V>, build: Build<V>, key: Key<V>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(owner, |this: &mut Self, owner, cx| {
            let key = (this.key)(owner.read(cx), cx);
            if key != this.built_from {
                cx.notify();
            }
        });
        Self {
            owner: owner.downgrade(),
            build,
            key,
            built_from: String::new(),
            subs: vec![sub],
        }
    }

    /// Also rebuilds the part whenever `other` is notified: a child entity the part draws, such as
    /// a text field, whose own notifications are not the owner's.
    pub fn also_observe<T: 'static>(&mut self, other: &Entity<T>, cx: &mut Context<Self>) {
        self.subs.push(cx.observe(other, |_, _, cx| cx.notify()));
    }
}

impl<V: 'static> Render for Facet<V> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(owner) = self.owner.upgrade() else {
            return div().into_any_element();
        };
        self.built_from = (self.key)(owner.read(cx), cx);
        let build = self.build;
        owner.update(cx, |owner, owner_cx| build(owner, window, owner_cx))
    }
}
