use std::time::Duration;

use dioxus::prelude::*;
use dioxus_primitives::dioxus_attributes::attributes;
use dioxus_primitives::merge_attributes;
use dioxus_primitives::toast;

/// The primitive's dispatch API, re-exported so that a caller needs one import.
///
/// [`use_toast`] is how anything under a [`ToastProvider`] shows a toast, and
/// [`ToastOptions`] is how it says what kind of toast it wants. Both are the
/// primitive's, unchanged.
pub use dioxus_primitives::toast::{ToastOptions, ToastType, Toasts, use_toast};

/// The props one toast is rendered from, re-exported for the same reason.
///
/// A caller who passes their own `render_toast` to [`ToastProvider`] is handed
/// [`ToastPropsWithOwner`] and writes a component over [`ToastProps`], the way
/// [`Toast`] is written, so both names are part of this component's surface
/// rather than the primitive's alone.
pub use dioxus_primitives::toast::{ToastProps, ToastPropsWithOwner};

/// daisyUI's inline axis, which is the edge of the viewport toasts
/// are pinned to across the writing direction.
///
/// Every value emits a class, the default included, following ADR-0008: an
/// unclassed `.toast` already sits at the inline end, so the class says where
/// the toasts go rather than leaving it to be read off an absence.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ToastInline {
    /// The inline start edge, which follows the writing direction: the left in
    /// a left-to-right document, the right in a right-to-left one.
    Start,
    Center,
    /// The inline end edge, mirroring [`ToastInline::Start`], and daisyUI's
    /// own default.
    #[default]
    End,
}

impl ToastInline {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[Self::Start, Self::Center, Self::End];

    /// The daisyUI class name for this value, as a complete string literal so
    /// Tailwind's scanner can see it.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Start => "toast-start",
            Self::Center => "toast-center",
            Self::End => "toast-end",
        }
    }
}

/// daisyUI's block axis, which is the edge of the viewport toasts are
/// pinned to along it.
///
/// Every value emits a class for the reason [`ToastInline`] records.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ToastBlock {
    Top,
    Middle,
    /// daisyUI's own default.
    #[default]
    Bottom,
}

impl ToastBlock {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[Self::Top, Self::Middle, Self::Bottom];

    /// The daisyUI class name for this value, as a complete string literal so
    /// Tailwind's scanner can see it.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Top => "toast-top",
            Self::Middle => "toast-middle",
            Self::Bottom => "toast-bottom",
        }
    }
}

/// daisyUI's colour axis for a toast, which is the alert colour its kind is
/// drawn in.
///
/// This is a **derived axis**: nobody passes it, because a toast's kind is
/// decided at the call that dispatches it (`toast.success(…)`,
/// `toast.error(…)`) and the element it styles is rendered by [`Toast`]
/// rather than by the caller. It is an axis in every other way: a class per
/// value, and a variant list the preview renders every one of.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ToastColor {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

impl ToastColor {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[Self::Info, Self::Success, Self::Warning, Self::Error];

    /// The daisyUI class name for this value, as a complete string literal so
    /// Tailwind's scanner can see it.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Info => "alert-info",
            Self::Success => "alert-success",
            Self::Warning => "alert-warning",
            Self::Error => "alert-error",
        }
    }

    /// The colour a toast of this kind is drawn in, which is the whole of the
    /// bridging: the primitive reports the kind as `data-type` and daisyUI
    /// matches no data attribute at all.
    pub const fn of(toast_type: ToastType) -> Self {
        match toast_type {
            ToastType::Info => Self::Info,
            ToastType::Success => Self::Success,
            ToastType::Warning => Self::Warning,
            ToastType::Error => Self::Error,
        }
    }

    /// The kind a toast of this colour is dispatched as, which is the way back
    /// (used by the preview to render every value of this axis).
    pub const fn toast_type(self) -> ToastType {
        match self {
            Self::Info => ToastType::Info,
            Self::Success => ToastType::Success,
            Self::Warning => ToastType::Warning,
            Self::Error => ToastType::Error,
        }
    }
}

/// Whether [`ToastProvider`] emits the utilities that lay the toasts out.
///
/// This is the axis ADR-0014 exists for. daisyUI's `.toast` stacks its **own
/// children** in a column and spaces them, and the primitive puts an `ol` and a
/// `li` between the container and each toast, so what daisyUI stacks is one
/// list rather than the toasts. The column and the gap are put back with
/// utilities that reach through the list, since neither element takes an
/// attribute from anywhere.
///
/// That inverts the usual convention: [`ToastListAppearance::Default`] emits
/// classes and [`ToastListAppearance::None`] emits nothing, so a caller who
/// wants a different arrangement switches ours off rather than trying to
/// out-rank a utility they only tie with (ADR-0004).
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ToastListAppearance {
    #[default]
    Default,
    None,
}

impl ToastListAppearance {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[Self::Default, Self::None];

    /// The Tailwind utilities for this value, as complete string literals so
    /// Tailwind's scanner can see them.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Default => "[&>ol]:flex [&>ol]:flex-col [&>ol]:gap-2",
            Self::None => "",
        }
    }
}

/// Whether [`ToastTitle`] emits the utilities that make it read as a title.
///
/// daisyUI has no class for an alert's title (the ones in its own examples are
/// Tailwind utilities) so they are emitted here, on the inverted convention
/// [`ToastListAppearance`] records (ADR-0004).
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ToastTitleAppearance {
    #[default]
    Default,
    None,
}

impl ToastTitleAppearance {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[Self::Default, Self::None];

    /// The Tailwind utilities for this value, as complete string literals so
    /// Tailwind's scanner can see them.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Default => "font-semibold",
            Self::None => "",
        }
    }
}

/// Whether [`ToastDescription`] emits the utilities that set it under the
/// title.
///
/// The inverse convention of the other axes, for the reason
/// [`ToastListAppearance`] records.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ToastDescriptionAppearance {
    #[default]
    Default,
    None,
}

impl ToastDescriptionAppearance {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[Self::Default, Self::None];

    /// The Tailwind utilities for this value, as complete string literals so
    /// Tailwind's scanner can see them.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Default => "text-xs",
            Self::None => "",
        }
    }
}

/// The region toasts are shown in, carrying daisyUI's `toast` classes.
///
/// Everything that dispatches a toast has to be under this component (that is
/// what [`use_toast`] reads) and there is normally one of them, wrapped around
/// the whole app. The toasts themselves are rendered through a portal, so where
/// they appear on screen is this component's classes rather than where it sits
/// in the tree.
///
/// There is no state to lift and nothing to mirror: the provider owns the queue
/// and hands each toast to [`Toast`] as props, which is where the kind becomes a
/// class.
///
/// What the appearance axis puts back, and why it has to, is ADR-0014.
///
/// Classes passed by the caller concatenate with the region's own; every other
/// attribute the caller passes overrides them.
#[component]
pub fn ToastProvider(
    /// daisyUI's inline axis.
    #[props(default)]
    inline: ToastInline,
    /// daisyUI's block axis.
    #[props(default)]
    block: ToastBlock,
    /// Whether to emit the utilities that lay the toasts out.
    #[props(default)]
    appearance: ToastListAppearance,
    /// How long a toast stays up when the call that dispatched it says nothing
    /// about it. The default repeats the primitive's own, since a prop declared
    /// here has to carry one.
    #[props(default = ReadSignal::new(Signal::new(Some(Duration::from_secs(5)))))]
    default_duration: ReadSignal<Option<Duration>>,
    /// How many toasts may be on screen at once. Beyond it the oldest
    /// dismissable one goes. The default repeats the primitive's own.
    #[props(default = ReadSignal::new(Signal::new(10)))]
    max_toasts: ReadSignal<usize>,
    /// How one toast is rendered. The default is [`Toast`], which is this
    /// registry's; a caller who wants different markup passes their own
    /// component the same way.
    #[props(default = Callback::new(|props: ToastPropsWithOwner| rsx! { Toast { ..props } }))]
    render_toast: Callback<ToastPropsWithOwner, Element>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let inline = inline.class();
    let block = block.class();
    let appearance = appearance.class();

    let base = attributes!(div {
        class: "toast {inline} {block} {appearance}",
    });
    let merged = merge_attributes(vec![base, attributes]);

    rsx! {
        toast::ToastProvider {
            default_duration,
            max_toasts,
            render_toast,
            attributes: merged,
            {children}
        }
    }
}

/// One toast, carrying daisyUI's `alert` classes.
///
/// This is where the kind is bridged, and it is **Tier 2**: the primitive
/// reports it as `data-type` on this element, and daisyUI's alert colours match
/// no data attribute, so the class is emitted from Rust, through
/// [`ToastColor::of`].
///
/// It takes the primitive's own props rather than props of its own, which is
/// what lets it be handed to [`ToastProvider`]'s `render_toast`: the provider
/// builds those props for every toast in its queue, and a component that took
/// anything else could not be given them.
///
/// The children are this registry's parts. A caller who wants others writes
/// their own component and passes it as `render_toast`.
///
/// The name is a component's rather than a function's, which is what the
/// `allow` is for: written with the `component` macro it could not have taken
/// the primitive's props type.
#[allow(non_snake_case)]
pub fn Toast(props: ToastProps) -> Element {
    let color = ToastColor::of(props.toast_type).class();

    let base = attributes!(div {
        class: "alert {color}",
    });
    let merged = merge_attributes(vec![base, props.attributes]);

    rsx! {
        toast::Toast {
            id: props.id,
            index: props.index,
            title: props.title,
            description: props.description,
            toast_type: props.toast_type,
            on_close: props.on_close,
            permanent: props.permanent,
            duration: props.duration,
            attributes: merged,
            ToastContent {
                ToastTitle {}
                ToastDescription {}
            }
            ToastCloseButton {}
        }
    }
}

/// The part of a toast that is announced, holding its title and description.
///
/// Nothing is emitted here. daisyUI's alert lays its children out itself (it
/// is a grid, and this is one cell of it) and the block box the primitive
/// renders stacks a title above a description without help.
///
/// The primitive gives this element `role="alert"` and `aria-atomic`, which is
/// what makes a toast announce itself on arrival, and the whole of it rather
/// than the word that changed.
#[component]
pub fn ToastContent(
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    rsx! {
        toast::ToastContent { attributes, {children} }
    }
}

/// A toast's title, which is what it is announced by.
///
/// With no children it renders the title the toast was dispatched with, which
/// is the usual case; children replace it.
#[component]
pub fn ToastTitle(
    /// Whether to emit the utilities that make this read as a title.
    #[props(default)]
    appearance: ToastTitleAppearance,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Option<Element>,
) -> Element {
    let appearance = appearance.class();

    let base = attributes!(div {
        class: "{appearance}",
    });
    let merged = merge_attributes(vec![base, attributes]);

    rsx! {
        toast::ToastTitle { attributes: merged, children }
    }
}

/// A toast's description, which is the line under the title.
///
/// A toast dispatched without one renders nothing here (the primitive returns
/// an empty element rather than an empty line) so this can be written
/// unconditionally.
#[component]
pub fn ToastDescription(
    /// Whether to emit the utilities that set this under the title.
    #[props(default)]
    appearance: ToastDescriptionAppearance,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Option<Element>,
) -> Element {
    let appearance = appearance.class();

    let base = attributes!(div {
        class: "{appearance}",
    });
    let merged = merge_attributes(vec![base, attributes]);

    rsx! {
        toast::ToastDescription { attributes: merged, children }
    }
}

/// The control that dismisses a toast, carrying the button's classes.
///
/// The class strings are the button component's, duplicated rather than
/// depended on: the registry uses no cross-component dependencies, and the
/// Tailwind contract already requires every one of these to be a literal in the
/// file that emits it. They are written here rather than exposed as axes
/// because this button is a toast's own chrome; a caller who wants another
/// look passes `class`, which concatenates.
///
/// Dismissing also returns focus to the toast region, which is the primitive's
/// doing: a keyboard user who closed the top toast is left where the next one
/// is rather than at the top of the document.
#[component]
pub fn ToastCloseButton(
    #[props(extends = GlobalAttributes)]
    #[props(extends = button)]
    attributes: Vec<Attribute>,
    children: Option<Element>,
) -> Element {
    let base = attributes!(button {
        class: "btn btn-ghost btn-xs btn-circle",
    });
    let merged = merge_attributes(vec![base, attributes]);

    rsx! {
        toast::ToastCloseButton { attributes: merged, children }
    }
}
