use crate::i18n::Language;

// ImageKind - used for classifying images during FastPic upload
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Screenlist,
    Screenshot,
    Other,
}

impl ImageKind {
    #[allow(dead_code)]
    pub fn label(self, language: Language) -> &'static str {
        match self {
            Self::Screenlist => language.text("screenlist"),
            Self::Screenshot => language.text("screenshot"),
            Self::Other => language.text("image"),
        }
    }
}
