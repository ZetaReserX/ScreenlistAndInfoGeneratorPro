// Модуль локализации

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Ru,
    En,
}

impl Language {
    #[inline]
    pub fn tr<'a>(self, ru: &'a str, en: &'a str) -> &'a str {
        match self {
            Self::Ru => ru,
            Self::En => en,
        }
    }

    pub fn text(self, key: &str) -> &'static str {
        match (self, key) {
            (Self::Ru, "subtitle") => "Скринлисты и скриншоты",
            (Self::En, "subtitle") => "Screenlists and screenshots",
            (Self::Ru, "add_images") => "Добавить изображения",
            (Self::En, "add_images") => "Add images",
            (Self::Ru, "add_folder") => "Добавить папку с изображениями",
            (Self::En, "add_folder") => "Add image folder",
            (Self::Ru, "clear") => "Очистить список",
            (Self::En, "clear") => "Clear list",
            (Self::Ru, "upload") => "Загрузить на FastPic",
            (Self::En, "upload") => "Upload to FastPic",
            (Self::Ru, "settings") => "Настройки FastPic",
            (Self::En, "settings") => "FastPic settings",
            (Self::Ru, "api_key") => "API-ключ",
            (Self::En, "api_key") => "API key",
            (Self::Ru, "own_key") => "Свой ключ",
            (Self::En, "own_key") => "Own key",
            (Self::Ru, "anonymous") => "Анонимная загрузка",
            (Self::En, "anonymous") => "Anonymous upload",
            (Self::Ru, "show") => "Показать",
            (Self::En, "show") => "Show",
            (Self::Ru, "hide") => "Скрыть",
            (Self::En, "hide") => "Hide",
            (Self::Ru, "thumbnail") => "Миниатюра",
            (Self::En, "thumbnail") => "Thumbnail",
            (Self::Ru, "make_thumbnail") => "Делать миниатюру",
            (Self::En, "make_thumbnail") => "Create thumbnail",
            (Self::Ru, "thumbnail_size") => "Большая сторона миниатюры, px",
            (Self::En, "thumbnail_size") => "Thumbnail longest side, px",
            (Self::Ru, "example_150") => "Например 150",
            (Self::En, "example_150") => "For example 150",
            (Self::Ru, "thumbnail_text") => "Текст миниатюры",
            (Self::En, "thumbnail_text") => "Thumbnail text",
            (Self::Ru, "no_text") => "Без текста",
            (Self::En, "no_text") => "No text",
            (Self::Ru, "size_mode") => "Разрешение и размер",
            (Self::En, "size_mode") => "Resolution and size",
            (Self::Ru, "custom_text") => "Кастомный текст",
            (Self::En, "custom_text") => "Custom text",
            (Self::Ru, "filename") => "Название файла",
            (Self::En, "filename") => "File name",
            (Self::Ru, "custom_thumbnail_text") => "Кастомный текст миниатюры",
            (Self::En, "custom_thumbnail_text") => "Custom thumbnail text",
            (Self::Ru, "delete_after") => "Удалить через, минут",
            (Self::En, "delete_after") => "Delete after, minutes",
            (Self::Ru, "no_delete") => "По умолчанию не удалять",
            (Self::En, "no_delete") => "Do not delete by default",
            (Self::Ru, "files") => "Файлы",
            (Self::En, "files") => "Files",
            (Self::Ru, "drop_files") => {
                "Перетащите сюда файлы или папки либо нажмите «Добавить изображения»."
            }
            (Self::En, "drop_files") => "Drop files or folders here, or click \"Add images\".",
            (Self::Ru, "results") => "Результаты",
            (Self::En, "results") => "Results",
            (Self::Ru, "results_empty") => "После загрузки здесь появятся ссылки FastPic.",
            (Self::En, "results_empty") => "FastPic links will appear here after uploading.",
            (Self::Ru, "open_page") => "Открыть страницу",
            (Self::En, "open_page") => "Open page",
            (Self::Ru, "copy") => "Копировать",
            (Self::En, "copy") => "Copy",
            (Self::Ru, "copy_all") => "Копировать все BBCode",
            (Self::En, "copy_all") => "Copy all BBCode",
            (Self::Ru, "copy_video") => "Копировать BBCode видео",
            (Self::En, "copy_video") => "Copy video BBCode",
            (Self::Ru, "expand_all") => "Развернуть все подразделы",
            (Self::En, "expand_all") => "Expand all subsections",
            (Self::Ru, "collapse_all") => "Свернуть все подразделы",
            (Self::En, "collapse_all") => "Collapse all subsections",
            (Self::Ru, "screenlist") => "скринлист",
            (Self::En, "screenlist") => "screenlist",
            (Self::Ru, "screenshot") => "скриншот",
            (Self::En, "screenshot") => "screenshot",
            (Self::Ru, "image") => "изображение",
            (Self::En, "image") => "image",
            (Self::Ru, "russian") => "Русский",
            (Self::En, "russian") => "Russian",
            (Self::Ru, "english") => "Английский",
            (Self::En, "english") => "English",
            (Self::Ru, "copy_full_bb_template") => "📋 Скопировать готовый BB-код (Шаблон релиза)",
            (Self::En, "copy_full_bb_template") => "📋 Copy Ready BBCode (Release Template)",
            (Self::Ru, "preview_full_bb_template") => "Показать готовый BB-код шаблона",
            (Self::En, "preview_full_bb_template") => "Show generated BBCode template",
            _ => "",
        }
    }
}
