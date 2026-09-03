use dioxus::prelude::*;
use dioxus_primitives::dioxus_attributes::attributes;
use dioxus_primitives::merge_attributes;

/// daisyUI's colour axis for a button.
///
/// [`ButtonColor::Default`] emits no class at all, which is daisyUI's own
/// uncoloured button rather than a synonym for [`ButtonColor::Neutral`].
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ButtonColor {
    #[default]
    Default,
    Neutral,
    Primary,
    Secondary,
    Accent,
    Info,
    Success,
    Warning,
    Error,
}

impl ButtonColor {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[
        Self::Default,
        Self::Neutral,
        Self::Primary,
        Self::Secondary,
        Self::Accent,
        Self::Info,
        Self::Success,
        Self::Warning,
        Self::Error,
    ];

    /// The daisyUI class name for this value, as a complete string literal so
    /// Tailwind's scanner can see it.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Neutral => "btn-neutral",
            Self::Primary => "btn-primary",
            Self::Secondary => "btn-secondary",
            Self::Accent => "btn-accent",
            Self::Info => "btn-info",
            Self::Success => "btn-success",
            Self::Warning => "btn-warning",
            Self::Error => "btn-error",
        }
    }
}

/// daisyUI's size axis for a button.
///
/// [`ButtonSize::Default`] emits no class, which renders at the same size as
/// daisyUI's explicit `btn-md`.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum ButtonSize {
    Xs,
    Sm,
    #[default]
    Default,
    Lg,
    Xl,
}

impl ButtonSize {
    /// Every value of this axis, in the order the preview renders them.
    pub const ALL: &'static [Self] = &[Self::Xs, Self::Sm, Self::Default, Self::Lg, Self::Xl];

    /// The daisyUI class name for this value, as a complete string literal so
    /// Tailwind's scanner can see it.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Xs => "btn-xs",
            Self::Sm => "btn-sm",
            Self::Default => "",
            Self::Lg => "btn-lg",
            Self::Xl => "btn-xl",
        }
    }
}

/// A button styled with daisyUI's `btn` classes.
///
/// Classes passed by the caller concatenate with the button's own; every other
/// attribute the caller passes overrides the button's.
#[component]
pub fn Button(
    /// daisyUI's colour axis.
    #[props(default)]
    color: ButtonColor,
    /// daisyUI's size axis.
    #[props(default)]
    size: ButtonSize,
    #[props(extends = GlobalAttributes)]
    #[props(extends = button)]
    attributes: Vec<Attribute>,
    /// Called when the button is clicked. `extends` reaches attributes only,
    /// never event handlers, so a handler has to be its own prop.
    onclick: Option<EventHandler<MouseEvent>>,
    children: Element,
) -> Element {
    let color = color.class();
    let size = size.class();

    let base = attributes!(button {
        class: "btn {color} {size}",
    });
    let merged = merge_attributes(vec![base, attributes]);

    rsx! {
        button {
            onclick: move |event| {
                if let Some(handler) = &onclick {
                    handler.call(event);
                }
            },
            ..merged,
            {children}
        }
    }
}
