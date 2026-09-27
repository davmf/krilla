//! PDF annotations, allowing you to add extra "content" to specific pages.
//!
//! PDF has the concept of annotations, which allow you to associate certain regions of
//! a page with an "annotation". The PDF reference defines many different actions, however,
//! krilla does not and never will expose all of them. As of right now, the supported
//! annotations are "link annotations", which allow you associate a certain region of
//! the page with a link, and "text annotations", which attach a note to a region.

use core::f32;

use pdf_writer::types::{AnnotationFlags, AnnotationIcon};
use pdf_writer::{Chunk, Finish, Name, Ref, TextStr};

use crate::chunk_container::ChunkContainer;
use crate::color::Color;
use crate::configure::{PdfVersion, ValidationError};
use crate::error::KrillaResult;
use crate::geom::{Quadrilateral, Rect};
use crate::interactive::action::Action;
use crate::interactive::destination::Destination;
use crate::page::page_root_transform;
use crate::serialize::SerializeContext;
use crate::surface::Location;

/// An annotation.
pub struct Annotation {
    pub(crate) annotation_type: AnnotationType,
    pub(crate) alt: Option<String>,
    pub(crate) struct_parent: Option<i32>,
    pub(crate) location: Option<Location>,
}

impl Annotation {
    /// Create a new link annotation with some alt text.
    ///
    /// Note that the alt text might be required in some cases, for example
    /// when exporting to PDF/UA.
    pub fn new_link(annotation: LinkAnnotation, alt_text: Option<String>) -> Self {
        Self {
            annotation_type: AnnotationType::Link(annotation),
            alt: alt_text,
            struct_parent: None,
            location: None,
        }
    }

    /// Create a new text annotation with some contents.
    ///
    /// Viewers typically show the contents as a note or tooltip.
    pub fn new_text(annotation: TextAnnotation, contents: String) -> Self {
        Self {
            annotation_type: AnnotationType::Text(annotation),
            alt: Some(contents),
            struct_parent: None,
            location: None,
        }
    }

    /// Sets the location of the annotation.
    pub fn with_location(mut self, location: Option<Location>) -> Self {
        self.location = location;
        self
    }
}

impl From<LinkAnnotation> for Annotation {
    fn from(value: LinkAnnotation) -> Self {
        Self {
            annotation_type: AnnotationType::Link(value),
            alt: None,
            struct_parent: None,
            location: None,
        }
    }
}

impl Annotation {
    pub(crate) fn serialize(
        &self,
        sc: &mut SerializeContext,
        chunk_container: &mut ChunkContainer,
        root_ref: Ref,
        page_height: f32,
    ) -> KrillaResult<()> {
        // Write the appearance stream first, since the annotation writer
        // borrows the annotation chunk.
        let appearance = match &self.annotation_type {
            AnnotationType::Text(t) if t.invisible => {
                let ap_ref = sc.new_ref();
                let rect = t.rect.transform(page_root_transform(page_height)).unwrap();
                let mut ap_chunk = Chunk::new();
                let mut x_object = ap_chunk.form_xobject(ap_ref, &[]);
                x_object.bbox(pdf_writer::Rect::new(0.0, 0.0, rect.width(), rect.height()));
                x_object.finish();
                chunk_container.streams.x_objects.push(ap_chunk);
                Some(ap_ref)
            }
            _ => None,
        };

        let chunk = &mut chunk_container.non_stream.annotations;
        let mut annotation = chunk
            .indirect(root_ref)
            .start::<pdf_writer::writers::Annotation>();

        self.annotation_type
            .serialize_type(sc, &mut annotation, appearance, page_height)?;

        let requires_flags = sc
            .serialize_settings()
            .configuration
            .validators()
            .requires_annotation_flags();
        match &self.annotation_type {
            AnnotationType::Link(l) => {
                // Only set the print flag when really necessary (only PDF/A).
                // Don't set it by default, so annotations with color borders
                // will be shown on a screen but not printed.
                // TODO: No need to write the print flag even if it is `None`,
                // only for PDF/A.
                if l.border.is_none() || requires_flags {
                    annotation.flags(AnnotationFlags::PRINT);
                }
            }
            AnnotationType::Text(t) => {
                if t.invisible {
                    // Nothing is drawn, so printing is harmless, and PDF/A
                    // requires the flag.
                    annotation.flags(AnnotationFlags::PRINT);
                } else if requires_flags {
                    annotation.flags(
                        AnnotationFlags::PRINT
                            | AnnotationFlags::NO_ZOOM
                            | AnnotationFlags::NO_ROTATE,
                    );
                } else {
                    // Keep the note icon off paper and at a fixed size.
                    annotation.flags(AnnotationFlags::NO_ZOOM | AnnotationFlags::NO_ROTATE);
                }
            }
        }

        if let Some(struct_parent) = self.struct_parent {
            annotation.struct_parent(struct_parent);
        }

        if let Some(alt_text) = &self.alt {
            annotation.contents(TextStr(alt_text));
        }

        if self.alt.as_ref().is_none_or(String::is_empty) {
            sc.register_validation_error(ValidationError::MissingAnnotationAltText(self.location));
        }

        annotation.finish();

        Ok(())
    }
}

/// A type of annotation.
pub enum AnnotationType {
    /// A link annotation.
    Link(LinkAnnotation),
    /// A text annotation.
    Text(TextAnnotation),
}

impl AnnotationType {
    fn serialize_type(
        &self,
        sc: &mut SerializeContext,
        annotation: &mut pdf_writer::writers::Annotation,
        appearance: Option<Ref>,
        page_height: f32,
    ) -> KrillaResult<()> {
        match self {
            AnnotationType::Link(l) => l.serialize_type(sc, annotation, page_height),
            AnnotationType::Text(t) => {
                t.serialize_type(annotation, appearance, page_height);
                Ok(())
            }
        }
    }
}

/// A text annotation, which attaches a note to a region of the page.
///
/// The note's text is the annotation's contents, passed to
/// [`Annotation::new_text`]. Viewers typically show it when hovering over the
/// annotation.
pub struct TextAnnotation {
    pub(crate) rect: Rect,
    pub(crate) invisible: bool,
}

impl TextAnnotation {
    /// Create a new text annotation.
    ///
    /// `rect`: The region of the page that the annotation should cover. For
    /// visible annotations, viewers draw a note icon at its top-left corner.
    pub fn new(rect: Rect) -> Self {
        Self {
            rect,
            invisible: false,
        }
    }

    /// Whether the annotation should be drawn without a note icon.
    ///
    /// Invisible text annotations are written with an empty appearance, so
    /// only their contents are shown, typically as a tooltip when hovering
    /// over `rect`.
    pub fn with_invisible(self, invisible: bool) -> Self {
        Self { invisible, ..self }
    }

    fn serialize_type(
        &self,
        annotation: &mut pdf_writer::writers::Annotation,
        appearance: Option<Ref>,
        page_height: f32,
    ) {
        annotation.subtype(pdf_writer::types::AnnotationType::Text);
        let actual_rect = self
            .rect
            .transform(page_root_transform(page_height))
            .unwrap();
        annotation.rect(actual_rect.to_pdf_rect());
        annotation.icon(AnnotationIcon::Comment);
        if let Some(ap_ref) = appearance {
            annotation.appearance().normal().stream(ap_ref);
        }
    }
}

/// An annotation target.
pub enum Target {
    /// A destination within the document.
    Destination(Destination),
    /// An action to be performed.
    Action(Action),
}

/// Border of a link annotation.
pub struct LinkBorder {
    pub(crate) width: f32,
    pub(crate) color: Color,
}

impl LinkBorder {
    /// Create a new link annotation border.
    ///
    /// `width`: The width of the border in pt.
    /// `color`: The color of the border.
    pub fn new(width: f32, color: Color) -> Self {
        Self { width, color }
    }
}

/// A link annotation.
pub struct LinkAnnotation {
    pub(crate) rect: Rect,
    pub(crate) quad_points: Option<Vec<Quadrilateral>>,
    pub(crate) target: Target,
    pub(crate) border: Option<LinkBorder>,
}

impl LinkAnnotation {
    /// Create a new link annotation.
    ///
    /// `rect`: The bounding box of the link annotation that it should cover on the page.
    /// `target`: The target of the link annotation.
    pub fn new(rect: Rect, target: Target) -> Self {
        Self {
            rect,
            quad_points: None,
            target,
            border: None,
        }
    }

    /// Create a new link annotation.
    ///
    /// `target`: The target of the link annotation.
    /// `quad_points`: An array of quadrilaterals that define where the link
    /// annotation should be activated. This is useful if you for example have
    /// a link annotation that is broken to one or multiple lines.
    pub fn new_with_quad_points(quad_points: Vec<Quadrilateral>, target: Target) -> Self {
        assert!(!quad_points.is_empty());

        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;

        for point in quad_points.iter().flat_map(|q| q.0) {
            min_x = min_x.min(point.x);
            min_y = min_y.min(point.y);
            max_x = max_x.max(point.x);
            max_y = max_y.max(point.y);
        }

        // Expand the bounding box by a little. There is a bug in adobe acrobat
        // that sometimes prevents the quadpoints from being used if the quad
        // points lie exactly on the bounding rectangle.
        const EPSILON: f32 = 0.001;
        let rect = Rect::from_ltrb(
            min_x - EPSILON,
            min_y - EPSILON,
            max_x + EPSILON,
            max_y + EPSILON,
        )
        .unwrap();

        Self {
            rect,
            quad_points: Some(quad_points),
            target,
            border: None,
        }
    }

    /// Set a border for this link annotation. The border will be visible on
    /// screen but not when printed, unless when exporting with PDF/A standard.
    pub fn with_border(self, border: LinkBorder) -> Self {
        Self {
            border: Some(border),
            ..self
        }
    }

    fn serialize_type(
        &self,
        sc: &mut SerializeContext,
        annotation: &mut pdf_writer::writers::Annotation,
        page_height: f32,
    ) -> KrillaResult<()> {
        annotation.subtype(pdf_writer::types::AnnotationType::Link);

        let actual_rect = self
            .rect
            .transform(page_root_transform(page_height))
            .unwrap();
        annotation.rect(actual_rect.to_pdf_rect());
        annotation.border(
            0.0,
            0.0,
            self.border.as_ref().map_or(0.0, |x| x.width),
            None,
        );

        if let Some(border) = &self.border {
            match border.color.to_regular() {
                crate::color::RegularColor::Rgb(rgb) => {
                    let [r, g, b] = rgb.to_pdf_color();
                    annotation.color_rgb(r, g, b);
                }
                crate::color::RegularColor::Cmyk(cmyk) => {
                    let [c, m, y, k] = cmyk.to_pdf_color();
                    annotation.color_cmyk(c, m, y, k);
                }
                crate::color::RegularColor::Luma(gray) => {
                    annotation.color_gray(gray.to_pdf_color());
                }
            }
        }

        if sc.serialize_settings().pdf_version() >= PdfVersion::Pdf16 {
            self.quad_points.as_ref().map(|p| {
                annotation.quad_points(p.iter().flat_map(|q| q.0).flat_map(|p| {
                    let mut p = p.to_tsp();
                    page_root_transform(page_height).to_tsp().map_point(&mut p);
                    [p.x, p.y]
                }))
            });
        }

        match &self.target {
            Target::Destination(destination) => {
                destination.serialize(sc, annotation.insert(Name(b"Dest")))
            }
            Target::Action(action) => action.serialize(sc, annotation.action()),
        }
    }
}
