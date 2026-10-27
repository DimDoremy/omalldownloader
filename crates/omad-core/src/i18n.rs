//! The two languages this program speaks, and every string it says.
//!
//! Strings live here rather than at the call sites for two reasons: a `Key` is a compile
//! error when it is misspelled, and every string has exactly one place where its
//! translation is missing. The language is `OMAD_LANG` when set (`en`, `zh`), otherwise
//! whatever `LC_ALL`/`LANG` say, otherwise English.

use std::fmt;

/// The languages the interface is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// The language to use: `OMAD_LANG` wins, then the POSIX locale variables.
    pub fn from_env() -> Self {
        if let Ok(explicit) = std::env::var("OMAD_LANG") {
            return Self::from_tag(&explicit).unwrap_or(Self::En);
        }
        for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(value) = std::env::var(name)
                && let Some(lang) = Self::from_tag(&value)
            {
                return lang;
            }
        }
        Self::En
    }

    /// `zh`, `zh_CN.UTF-8`, `zh-Hans` and friends mean Chinese; anything else is English.
    pub fn from_tag(tag: &str) -> Option<Self> {
        let tag = tag.trim().to_ascii_lowercase();
        if tag.is_empty() || tag == "c" || tag == "posix" {
            return None;
        }
        if tag.starts_with("zh") {
            return Some(Self::Zh);
        }
        if tag.starts_with("en") {
            return Some(Self::En);
        }
        None
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Zh => "zh",
        }
    }
}

/// Every string the window and the command line show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    NavTasks,
    NavAdd,
    NavExtension,
    NavSettings,
    FilterAll,
    FilterActive,
    FilterDone,
    StartAll,
    PauseAll,
    SearchTasks,
    FilterTasks,
    StatusQueued,
    StatusRunning,
    StatusPaused,
    StatusCompleted,
    StatusFailed,
    ActionOpen,
    ActionFolder,
    ActionRemove,
    ActionPause,
    ActionResume,
    PageAddTitle,
    AddPlaceholder,
    AddButton,
    AddCancel,
    SavingTo,
    PageExtensionTitle,
    ExtensionExplains,
    ExtensionAddress,
    ExtensionNoBrowser,
    ExtensionOneBrowser,
    ExtensionManyBrowsers,
    ExtensionLoadingTitle,
    ExtensionLoadingText,
    PageSettingsTitle,
    NoTasks,
    NoResults,
    SpeedPerSecond,
    CliNoTasks,
    CliAdded,
    CliCompleted,
    CliPaused,
    CliSavedTo,
    CliRemoved,
    CliHandedTo,
    ColId,
    ColState,
    ColProgress,
    ColName,
}

impl Key {
    pub fn text(self, lang: Lang) -> &'static str {
        use Key::*;
        match (self, lang) {
            (NavTasks, Lang::En) => "Tasks",
            (NavTasks, Lang::Zh) => "任务",
            (NavAdd, Lang::En) => "Add a task",
            (NavAdd, Lang::Zh) => "新建任务",
            (NavExtension, Lang::En) => "Extension",
            (NavExtension, Lang::Zh) => "浏览器扩展",
            (NavSettings, Lang::En) => "Settings",
            (NavSettings, Lang::Zh) => "设置",

            (FilterAll, Lang::En) => "All",
            (FilterAll, Lang::Zh) => "全部",
            (FilterActive, Lang::En) => "Active",
            (FilterActive, Lang::Zh) => "进行中",
            (FilterDone, Lang::En) => "Done",
            (FilterDone, Lang::Zh) => "已完成",
            (StartAll, Lang::En) => "Start all",
            (StartAll, Lang::Zh) => "全部开始",
            (PauseAll, Lang::En) => "Pause all",
            (PauseAll, Lang::Zh) => "全部暂停",
            (SearchTasks, Lang::En) => "Search tasks",
            (SearchTasks, Lang::Zh) => "搜索任务",
            (FilterTasks, Lang::En) => "Filter tasks",
            (FilterTasks, Lang::Zh) => "筛选任务",

            (StatusQueued, Lang::En) => "Queued",
            (StatusQueued, Lang::Zh) => "排队中",
            (StatusRunning, Lang::En) => "Downloading",
            (StatusRunning, Lang::Zh) => "下载中",
            (StatusPaused, Lang::En) => "Paused",
            (StatusPaused, Lang::Zh) => "已暂停",
            (StatusCompleted, Lang::En) => "Done",
            (StatusCompleted, Lang::Zh) => "已完成",
            (StatusFailed, Lang::En) => "Failed",
            (StatusFailed, Lang::Zh) => "失败",

            (ActionOpen, Lang::En) => "Open",
            (ActionOpen, Lang::Zh) => "打开",
            (ActionFolder, Lang::En) => "Folder",
            (ActionFolder, Lang::Zh) => "所在文件夹",
            (ActionRemove, Lang::En) => "Remove",
            (ActionRemove, Lang::Zh) => "移除",
            (ActionPause, Lang::En) => "Pause",
            (ActionPause, Lang::Zh) => "暂停",
            (ActionResume, Lang::En) => "Resume",
            (ActionResume, Lang::Zh) => "继续",

            (PageAddTitle, Lang::En) => "Add a task",
            (PageAddTitle, Lang::Zh) => "新建下载任务",
            (AddPlaceholder, Lang::En) => "Paste a link. The server decides the name.",
            (AddPlaceholder, Lang::Zh) => "粘贴链接。文件名由服务器决定。",
            (AddButton, Lang::En) => "Add",
            (AddButton, Lang::Zh) => "添加",
            (AddCancel, Lang::En) => "Cancel",
            (AddCancel, Lang::Zh) => "取消",
            (SavingTo, Lang::En) => "Saving to {}",
            (SavingTo, Lang::Zh) => "保存到 {}",

            (PageExtensionTitle, Lang::En) => "Browser extension",
            (PageExtensionTitle, Lang::Zh) => "浏览器扩展",
            (ExtensionExplains, Lang::En) => {
                "The extension connects by itself: it looks for this address, and the first \
                 time it does, this app trusts it and hands it a secret. Nothing to type. A \
                 web page cannot take that place - only a browser extension origin is accepted."
            }
            (ExtensionExplains, Lang::Zh) => {
                "扩展会自动连接：它查找下面这个地址，第一次连上时本应用就信任它并发放密钥，\
                 无需输入任何内容。网页无法冒充，只有浏览器扩展的来源会被接受。"
            }
            (ExtensionAddress, Lang::En) => "Address",
            (ExtensionAddress, Lang::Zh) => "地址",
            (ExtensionNoBrowser, Lang::En) => "No browser has connected yet.",
            (ExtensionNoBrowser, Lang::Zh) => "还没有浏览器连接。",
            (ExtensionOneBrowser, Lang::En) => "1 browser connected.",
            (ExtensionOneBrowser, Lang::Zh) => "已连接 1 个浏览器。",
            (ExtensionManyBrowsers, Lang::En) => "{} browsers connected.",
            (ExtensionManyBrowsers, Lang::Zh) => "已连接 {} 个浏览器。",
            (ExtensionLoadingTitle, Lang::En) => "Loading the extension",
            (ExtensionLoadingTitle, Lang::Zh) => "如何加载扩展",
            (ExtensionLoadingText, Lang::En) => {
                "Edge or Chrome: edge://extensions, turn on Developer mode, Load unpacked, and \
                 choose ~/.local/share/omalldownloader/extension."
            }
            (ExtensionLoadingText, Lang::Zh) => {
                "Edge 或 Chrome：打开 edge://extensions，启用开发者模式，点击“加载解压缩的扩展”，\
                 选择 ~/.local/share/omalldownloader/extension。"
            }

            (PageSettingsTitle, Lang::En) => "Settings",
            (PageSettingsTitle, Lang::Zh) => "设置",
            (NoTasks, Lang::En) => "No downloads yet.",
            (NoTasks, Lang::Zh) => "还没有下载任务。",
            (NoResults, Lang::En) => "Nothing matches that filter.",
            (NoResults, Lang::Zh) => "没有符合筛选条件的任务。",
            (SpeedPerSecond, Lang::En) => "{}/s",
            (SpeedPerSecond, Lang::Zh) => "{}/秒",

            (CliNoTasks, Lang::En) => "no tasks",
            (CliNoTasks, Lang::Zh) => "没有任务",
            (CliAdded, Lang::En) => "added",
            (CliAdded, Lang::Zh) => "已添加",
            (CliCompleted, Lang::En) => "completed",
            (CliCompleted, Lang::Zh) => "已完成",
            (CliPaused, Lang::En) => "paused",
            (CliPaused, Lang::Zh) => "已暂停",
            (CliSavedTo, Lang::En) => "saved to",
            (CliSavedTo, Lang::Zh) => "已保存到",
            (CliRemoved, Lang::En) => "removed",
            (CliRemoved, Lang::Zh) => "已移除",
            (CliHandedTo, Lang::En) => "handed to",
            (CliHandedTo, Lang::Zh) => "已交给",
            (ColId, Lang::En) => "ID",
            (ColId, Lang::Zh) => "编号",
            (ColState, Lang::En) => "STATE",
            (ColState, Lang::Zh) => "状态",
            (ColProgress, Lang::En) => "PROGRESS",
            (ColProgress, Lang::Zh) => "进度",
            (ColName, Lang::En) => "NAME",
            (ColName, Lang::Zh) => "名称",
        }
    }

    /// The translation, with `{}` replaced by the one argument these strings take.
    pub fn with(self, lang: Lang, argument: impl fmt::Display) -> String {
        self.text(lang).replacen("{}", &argument.to_string(), 1)
    }
}

/// `t!(lang, Key)` — shorter at the call sites, and still checked by the compiler.
#[macro_export]
macro_rules! t {
    ($lang:expr, $key:ident) => {
        $crate::i18n::Key::$key.text($lang)
    };
    ($lang:expr, $key:ident, $arg:expr) => {
        $crate::i18n::Key::$key.with($lang, $arg)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_locale_variable_decides_the_language() {
        assert_eq!(Lang::from_tag("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::from_tag("zh-Hans"), Some(Lang::Zh));
        assert_eq!(Lang::from_tag("en_GB.UTF-8"), Some(Lang::En));
        assert_eq!(Lang::from_tag("C"), None);
        assert_eq!(Lang::from_tag(""), None);
        // An unknown locale falls back rather than guessing.
        assert_eq!(Lang::from_tag("fr_FR.UTF-8"), None);
    }

    #[test]
    fn every_string_says_something_in_both_languages() {
        use Key::*;
        let keys = [
            NavTasks,
            NavAdd,
            NavExtension,
            NavSettings,
            FilterAll,
            FilterActive,
            FilterDone,
            StartAll,
            PauseAll,
            SearchTasks,
            FilterTasks,
            StatusQueued,
            StatusRunning,
            StatusPaused,
            StatusCompleted,
            StatusFailed,
            ActionOpen,
            ActionFolder,
            ActionRemove,
            ActionPause,
            ActionResume,
            PageAddTitle,
            AddPlaceholder,
            AddButton,
            AddCancel,
            SavingTo,
            PageExtensionTitle,
            ExtensionExplains,
            ExtensionAddress,
            ExtensionNoBrowser,
            ExtensionOneBrowser,
            ExtensionManyBrowsers,
            ExtensionLoadingTitle,
            ExtensionLoadingText,
            PageSettingsTitle,
            NoTasks,
            NoResults,
            SpeedPerSecond,
            CliNoTasks,
            CliAdded,
            CliCompleted,
            CliPaused,
            CliSavedTo,
            CliRemoved,
            CliHandedTo,
            ColId,
            ColState,
            ColProgress,
            ColName,
        ];
        for key in keys {
            let en = key.text(Lang::En);
            let zh = key.text(Lang::Zh);
            assert!(!en.trim().is_empty(), "{key:?} has no English");
            assert!(!zh.trim().is_empty(), "{key:?} has no Chinese");
            assert_ne!(en, zh, "{key:?} was not translated");
        }
    }

    #[test]
    fn an_argument_is_placed_once() {
        assert_eq!(
            Key::ExtensionManyBrowsers.with(Lang::Zh, 3),
            "已连接 3 个浏览器。"
        );
        assert_eq!(Key::SavingTo.with(Lang::En, "/tmp"), "Saving to /tmp");
    }
}
