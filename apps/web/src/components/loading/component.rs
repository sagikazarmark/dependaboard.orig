use dioxus::prelude::*;
use dioxus_primitives::dioxus_attributes::attributes;
use dioxus_primitives::merge_attributes;

/// daisyUI's animation axis for a loading indicator.
///
/// [`LoadingAnimation::Default`] emits no modifier because the unclassed
/// `loading` indicator is the spinner.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum LoadingAnimation {
    #[default]
    Default,
    Dots,
    Ring,
    Ball,
    Bars,
    Infinity,
}

impl LoadingAnimation {
    /// Every value of this Axis, in the order the Preview renders them.
    pub const ALL: &'static [Self] = &[
        Self::Default,
        Self::Dots,
        Self::Ring,
        Self::Ball,
        Self::Bars,
        Self::Infinity,
    ];

    /// The daisyUI class name for this value, as a complete string literal so
    /// Tailwind's scanner can see it.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Dots => "loading-dots",
            Self::Ring => "loading-ring",
            Self::Ball => "loading-ball",
            Self::Bars => "loading-bars",
            Self::Infinity => "loading-infinity",
        }
    }
}

/// daisyUI's size axis for a loading indicator.
///
/// [`LoadingSize::Default`] emits no modifier and renders at the same medium
/// size as daisyUI's explicit `loading-md`.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[non_exhaustive]
pub enum LoadingSize {
    Xs,
    Sm,
    #[default]
    Default,
    Lg,
    Xl,
}

impl LoadingSize {
    /// Every value of this Axis, from smallest to largest.
    pub const ALL: &'static [Self] = &[Self::Xs, Self::Sm, Self::Default, Self::Lg, Self::Xl];

    /// The daisyUI class name for this value, as a complete string literal so
    /// Tailwind's scanner can see it.
    pub const fn class(self) -> &'static str {
        match self {
            Self::Xs => "loading-xs",
            Self::Sm => "loading-sm",
            Self::Default => "",
            Self::Lg => "loading-lg",
            Self::Xl => "loading-xl",
        }
    }
}

/// An empty loading indicator styled with daisyUI's `loading` classes.
///
/// Colour is `currentColor`, so callers paint the indicator with a text-colour
/// utility. Accessibility semantics also belong to the caller: a decorative
/// indicator and an announced operation require different attributes.
/// Classes passed by the caller concatenate with the indicator's own; every
/// other attribute the caller passes overrides the indicator's.
#[component]
pub fn Loading(
    /// daisyUI's animation axis, which is the shape the indicator draws.
    #[props(default)]
    animation: LoadingAnimation,
    /// daisyUI's size axis.
    #[props(default)]
    size: LoadingSize,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
) -> Element {
    let animation = animation.class();
    let size = size.class();

    let base = attributes!(span {
        class: "loading {animation} {size}",
    });
    let merged = merge_attributes(vec![base, attributes]);

    rsx! {
        span { ..merged }
    }
}
