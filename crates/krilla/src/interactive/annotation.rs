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
use crate::color::{rgb, Color};
use crate::configure::{PdfVersion, ValidationError};
use crate::error::KrillaResult;
use crate::geom::{PathBuilder, Quadrilateral, Rect};
use crate::graphics::xobject::XObject;
use crate::interactive::action::Action;
use crate::interactive::destination::Destination;
use crate::metadata::{pdf_date, DateTime};
use crate::num::NormalizedF32;
use crate::page::page_root_transform;
use crate::paint::{Fill, Stroke};
use crate::serialize::SerializeContext;
use crate::stream::StreamBuilder;
use crate::surface::Location;

/// The size of the drawn note icon, in pt. Viewers scale it to the
/// annotation's rect.
const NOTE_ICON_SIZE: f32 = 20.0;

/// An annotation.
pub struct Annotation {
    pub(crate) annotation_type: AnnotationType,
    pub(crate) alt: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) subject: Option<String>,
    pub(crate) modified: Option<DateTime>,
    pub(crate) name: Option<String>,
    pub(crate) color: Option<Color>,
    pub(crate) opacity: NormalizedF32,
    pub(crate) printable: Option<bool>,
    pub(crate) read_only: bool,
    pub(crate) locked: bool,
    pub(crate) struct_parent: Option<i32>,
    pub(crate) location: Option<Location>,
}

impl Annotation {
    fn new(annotation_type: AnnotationType, alt: Option<String>) -> Self {
        Self {
            annotation_type,
            alt,
            author: None,
            subject: None,
            modified: None,
            name: None,
            color: None,
            opacity: NormalizedF32::ONE,
            printable: None,
            read_only: false,
            locked: false,
            struct_parent: None,
            location: None,
        }
    }

    /// Create a new link annotation with some alt text.
    ///
    /// Note that the alt text might be required in some cases, for example
    /// when exporting to PDF/UA.
    pub fn new_link(annotation: LinkAnnotation, alt_text: Option<String>) -> Self {
        Self::new(AnnotationType::Link(annotation), alt_text)
    }

    /// Create a new text annotation with some contents.
    ///
    /// Viewers typically show the contents as a note or tooltip.
    pub fn new_text(annotation: TextAnnotation, contents: String) -> Self {
        Self::new(AnnotationType::Text(annotation), Some(contents))
    }

    /// Sets the location of the annotation.
    pub fn with_location(mut self, location: Option<Location>) -> Self {
        self.location = location;
        self
    }

    /// Sets the author of the annotation.
    ///
    /// Viewers typically show it as the title of the note.
    pub fn with_author(mut self, author: Option<String>) -> Self {
        self.author = author;
        self
    }

    /// Sets a short description of the annotation's subject.
    pub fn with_subject(mut self, subject: Option<String>) -> Self {
        self.subject = subject;
        self
    }

    /// Sets the date and time at which the annotation was last modified.
    pub fn with_modified(mut self, modified: Option<DateTime>) -> Self {
        self.modified = modified;
        self
    }

    /// Sets a name that uniquely identifies the annotation on its page.
    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }

    /// Sets the color of the annotation.
    ///
    /// For text annotations, this is the color of the note icon. It is ignored
    /// for link annotations, whose color is set with [`LinkBorder`].
    pub fn with_color(mut self, color: Option<Color>) -> Self {
        self.color = color;
        self
    }

    /// Sets the opacity of the annotation.
    ///
    /// Opacities below one require transparency, which is not allowed in
    /// PDF/A-1.
    pub fn with_opacity(mut self, opacity: NormalizedF32) -> Self {
        self.opacity = opacity;
        self
    }

    /// Sets whether the annotation is printed.
    ///
    /// If unset, a default suitable for the annotation type is used. Some
    /// standards, such as PDF/A, require all annotations to be printable.
    pub fn with_printable(mut self, printable: Option<bool>) -> Self {
        self.printable = printable;
        self
    }

    /// Sets whether users may interact with the annotation, for example by
    /// opening it.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Sets whether users may delete the annotation or change its properties.
    pub fn with_locked(mut self, locked: bool) -> Self {
        self.locked = locked;
        self
    }
}

impl From<LinkAnnotation> for Annotation {
    fn from(value: LinkAnnotation) -> Self {
        Self::new(AnnotationType::Link(value), None)
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
            AnnotationType::Text(t) => {
                Some(t.write_appearance(sc, chunk_container, self.color.clone(), page_height))
            }
            AnnotationType::Link(_) => None,
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
        let mut flags = match &self.annotation_type {
            AnnotationType::Link(l) => {
                // Only set the print flag when really necessary (only PDF/A).
                // Don't set it by default, so annotations with color borders
                // will be shown on a screen but not printed.
                // TODO: No need to write the print flag even if it is `None`,
                // only for PDF/A.
                if l.border.is_none() || requires_flags {
                    AnnotationFlags::PRINT
                } else {
                    AnnotationFlags::empty()
                }
            }
            AnnotationType::Text(t) => {
                if t.invisible {
                    // Nothing is drawn, so printing is harmless, and PDF/A
                    // requires the flag.
                    AnnotationFlags::PRINT
                } else if requires_flags {
                    AnnotationFlags::PRINT | AnnotationFlags::NO_ZOOM | AnnotationFlags::NO_ROTATE
                } else {
                    // Keep the note icon off paper and at a fixed size.
                    AnnotationFlags::NO_ZOOM | AnnotationFlags::NO_ROTATE
                }
            }
        };
        match self.printable {
            _ if requires_flags => {}
            Some(true) => flags |= AnnotationFlags::PRINT,
            Some(false) => flags -= AnnotationFlags::PRINT,
            None => {}
        }
        if self.read_only {
            flags |= AnnotationFlags::READ_ONLY;
        }
        if self.locked {
            flags |= AnnotationFlags::LOCKED;
        }
        if !flags.is_empty() {
            annotation.flags(flags);
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

        if let Some(author) = &self.author {
            annotation.author(TextStr(author));
        }

        if let Some(subject) = &self.subject {
            annotation.subject(TextStr(subject));
        }

        if let Some(modified) = self.modified {
            annotation.modified(pdf_date(modified));
        }

        if let Some(name) = &self.name {
            annotation.name(TextStr(name));
        }

        if !matches!(self.annotation_type, AnnotationType::Link(_)) {
            if let Some(color) = &self.color {
                write_color(&mut annotation, color);
            }
        }

        if self.opacity != NormalizedF32::ONE {
            sc.register_validation_error(ValidationError::Transparency(self.location));
            annotation.insert(Name(b"CA")).primitive(self.opacity.get());
        }

        annotation.finish();

        Ok(())
    }
}

/// Write the `/C` entry of an annotation.
fn write_color(annotation: &mut pdf_writer::writers::Annotation, color: &Color) {
    match color.to_regular() {
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

/// The icon of a visible text annotation.
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq, Hash)]
pub enum NoteIcon {
    /// A speech bubble.
    #[default]
    Comment,
    /// A sheet of paper.
    Note,
    /// A help sign.
    Help,
    /// A key.
    Key,
    /// An insertion caret.
    Insert,
    /// A paragraph sign.
    Paragraph,
    /// A new paragraph sign.
    NewParagraph,
}

impl NoteIcon {
    fn to_pdf(self) -> AnnotationIcon<'static> {
        match self {
            NoteIcon::Comment => AnnotationIcon::Comment,
            NoteIcon::Note => AnnotationIcon::Note,
            NoteIcon::Help => AnnotationIcon::Help,
            NoteIcon::Key => AnnotationIcon::Key,
            NoteIcon::Insert => AnnotationIcon::Insert,
            NoteIcon::Paragraph => AnnotationIcon::Paragraph,
            NoteIcon::NewParagraph => AnnotationIcon::NewParagraph,
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
    pub(crate) icon: NoteIcon,
    pub(crate) open: bool,
}

impl TextAnnotation {
    /// Create a new text annotation.
    ///
    /// `rect`: The region of the page that the annotation should cover. For
    /// visible annotations, the note icon fills this region, so it should be
    /// roughly square. 20pt is a typical size.
    pub fn new(rect: Rect) -> Self {
        Self {
            rect,
            invisible: false,
            icon: NoteIcon::default(),
            open: false,
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

    /// Sets the icon of a visible annotation.
    ///
    /// krilla draws a speech bubble for [`NoteIcon::Comment`] and a sheet of
    /// paper for all other icons. Viewers that draw their own icons may use
    /// the icon's specific symbol.
    pub fn with_icon(self, icon: NoteIcon) -> Self {
        Self { icon, ..self }
    }

    /// Whether the note should initially be shown open.
    pub fn with_open(self, open: bool) -> Self {
        Self { open, ..self }
    }

    /// The annotation's rectangle in PDF coordinates.
    fn pdf_rect(&self, page_height: f32) -> Rect {
        self.rect
            .transform(page_root_transform(page_height))
            .unwrap()
    }

    /// Write the annotation's appearance stream and return its reference.
    fn write_appearance(
        &self,
        sc: &mut SerializeContext,
        chunk_container: &mut ChunkContainer,
        color: Option<Color>,
        page_height: f32,
    ) -> Ref {
        if self.invisible {
            let ap_ref = sc.new_ref();
            let rect = self.pdf_rect(page_height);
            let mut ap_chunk = Chunk::new();
            let mut x_object = ap_chunk.form_xobject(ap_ref, &[]);
            x_object.bbox(pdf_writer::Rect::new(0.0, 0.0, rect.width(), rect.height()));
            x_object.finish();
            chunk_container.streams.x_objects.push(ap_chunk);
            return ap_ref;
        }

        let color = color.unwrap_or_else(|| rgb::Color::new(255, 214, 51).into());
        let mut builder = StreamBuilder::new(sc, chunk_container);
        let mut surface = builder.surface();
        // Appearance streams are drawn in PDF coordinates, so y points up.
        let mut body = PathBuilder::new();
        let mut lines = PathBuilder::new();
        if self.icon == NoteIcon::Comment {
            // A speech bubble with a tail at the bottom left.
            body.move_to(2.0, 18.5);
            body.line_to(18.0, 18.5);
            body.line_to(18.0, 6.5);
            body.line_to(9.0, 6.5);
            body.line_to(4.5, 2.0);
            body.line_to(4.5, 6.5);
            body.line_to(2.0, 6.5);
            body.close();
            for y in [15.0, 12.5, 10.0] {
                lines.move_to(5.0, y);
                lines.line_to(15.0, y);
            }
        } else {
            // A sheet of paper with a folded corner.
            body.move_to(3.5, 19.0);
            body.line_to(12.5, 19.0);
            body.line_to(16.5, 15.0);
            body.line_to(16.5, 1.0);
            body.line_to(3.5, 1.0);
            body.close();
            for y in [12.5, 9.5, 6.5] {
                lines.move_to(6.0, y);
                lines.line_to(14.0, y);
            }
        }
        let stroke = Stroke {
            width: 0.75,
            ..Stroke::default()
        };
        surface.set_fill(Some(Fill {
            paint: color.into(),
            ..Fill::default()
        }));
        surface.set_stroke(Some(stroke.clone()));
        surface.draw_path(&body.finish().unwrap());
        surface.set_fill(None);
        surface.set_stroke(Some(Stroke {
            width: 0.5,
            ..stroke
        }));
        surface.draw_path(&lines.finish().unwrap());
        surface.finish();
        let stream = builder.finish();

        let bbox = Rect::from_xywh(0.0, 0.0, NOTE_ICON_SIZE, NOTE_ICON_SIZE).unwrap();
        let x_object = XObject::new(stream, false, false, Some(bbox));
        sc.register_cacheable(chunk_container, x_object)
    }

    fn serialize_type(
        &self,
        annotation: &mut pdf_writer::writers::Annotation,
        appearance: Option<Ref>,
        page_height: f32,
    ) {
        annotation.subtype(pdf_writer::types::AnnotationType::Text);
        annotation.rect(self.pdf_rect(page_height).to_pdf_rect());
        annotation.icon(self.icon.to_pdf());
        if self.open {
            annotation.insert(Name(b"Open")).primitive(true);
        }
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
            write_color(annotation, &border.color);
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
