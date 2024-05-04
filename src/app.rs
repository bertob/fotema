// SPDX-FileCopyrightText: © 2024 David Bliss
//
// SPDX-License-Identifier: GPL-3.0-or-later

use relm4::{
    actions::{RelmAction, RelmActionGroup},
    adw,
    adw::prelude::{AdwApplicationWindowExt, NavigationPageExt},
    component::{AsyncComponent, AsyncComponentController},
    gtk,
    gtk::{
        gio, glib,
        prelude::{
            ApplicationExt, ApplicationWindowExt, ButtonExt, GtkWindowExt, OrientableExt,
            SettingsExt, WidgetExt,
        },
    },
    main_application,
    prelude::AsyncController,
    Component, ComponentController, ComponentParts, ComponentSender, Controller, RelmWidgetExt,
    SimpleComponent, WorkerController,
};

use relm4;

use crate::config::{APP_ID, PROFILE};
use fotema_core::database;
use fotema_core::video;
use fotema_core::VisualId;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::str::FromStr;

use strum::EnumString;
use strum::IntoStaticStr;

mod components;

use self::components::{
    about::AboutDialog,
    album::{Album, AlbumFilter, AlbumInput, AlbumOutput},
    folder_photos::{FolderPhotos, FolderPhotosInput, FolderPhotosOutput},
    library::{Library, LibraryInput, LibraryOutput},
    one_photo::{OnePhoto, OnePhotoInput},
    photo_info::PhotoInfo,
    preferences::{PreferencesDialog, PreferencesInput, PreferencesOutput},

};

mod background;

use self::background::bootstrap::{
    Bootstrap, BootstrapInput, BootstrapOutput,
};

// Visual items to be shared between various views.
// State is loaded by the `load_library` background task.
type SharedState = Arc<relm4::SharedState<Vec<Arc<fotema_core::Visual>>>>;

#[derive(Debug, Eq, PartialEq, EnumString, IntoStaticStr)]
pub enum ViewName {
    Nothing, // no view
    Library, // parent of all, month, and year views.
    Videos,
    Animated,
    Folders,
    Folder,
    Selfies,
}

pub(super) struct App {
    about_dialog: Controller<AboutDialog>,
    preferences_dialog: Controller<PreferencesDialog>,

    bootstrap: WorkerController<Bootstrap>,

    library: Controller<Library>,

    one_photo: AsyncController<OnePhoto>,

    show_selfies: bool,
    selfies_page: Controller<Album>,
    videos_page: Controller<Album>,
    motion_page: Controller<Album>,

    // Grid of folders of photos
    folder_photos: Controller<FolderPhotos>,

    // Folder album currently being viewed
    folder_album: Controller<Album>,

    // Main navigation. Parent of library stack.
    main_navigation: adw::OverlaySplitView,

    // Stack containing Library, Selfies, Folders, etc.
    main_stack: gtk::Stack,

    // Library pages
    library_view_stack: adw::ViewStack,

    // Switch between library views and single image view.
    picture_navigation_view: adw::NavigationView,

    // Window header bar
    header_bar: adw::HeaderBar,

    // Activity indicator. Only shown when progress bar is hidden.
    spinner: gtk::Spinner,

    // TODO there are too many progress_* fields. Move to a custom Progress component?

    // Progress indicator.
    progress_bar: gtk::ProgressBar,

    // Container for related progress bar components
    progress_box: gtk::Box,

    // Expected number of items we are recording progress for
    progress_end_count: usize,

    // Number of items processed so far.
    progress_current_count: usize,

    // Message banner
    banner: adw::Banner,
}

#[derive(Debug)]
pub(super) enum AppMsg {
    Quit,

    // Toggle visibility of sidebar
    ToggleSidebar,

    // A sidebar item has been clicked
    SwitchView,

    // Show item.
    ViewPhoto(VisualId),

    // Shown item is dismissed.
    ViewHidden,

    ViewFolder(PathBuf),

    // A task that can make progress has started.
    // count of items, banner text, progress bar text
    ProgressStarted(usize, String, String),

    // One item has been processed
    ProgressAdvanced,

    // Finished processing
    ProgressCompleted,

    // A task (without a progress bar) has started
    TaskStarted(String),

    // Preferences
    PreferencesUpdated,

    // All background bootstrap tasks have completed
    BootstrapCompleted,
}

relm4::new_action_group!(pub(super) WindowActionGroup, "win");
relm4::new_stateless_action!(PreferencesAction, WindowActionGroup, "preferences");
relm4::new_stateless_action!(pub(super) ShortcutsAction, WindowActionGroup, "show-help-overlay");
relm4::new_stateless_action!(AboutAction, WindowActionGroup, "about");

#[relm4::component(pub)]
impl SimpleComponent for App {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type Widgets = AppWidgets;

    menu! {
        primary_menu: {
            section! {
                "_Preferences" => PreferencesAction,
                "_Keyboard" => ShortcutsAction,
                "_About Fotema" => AboutAction,
            }
        }
    }

    view! {
        main_window = adw::ApplicationWindow::new(&main_application()) {
            set_visible: true,
            set_width_request: 400,
            set_height_request: 400,

            connect_close_request[sender] => move |_| {
                sender.input(AppMsg::Quit);
                glib::Propagation::Stop
            },

            #[wrap(Some)]
            set_help_overlay: shortcuts = &gtk::Builder::from_resource(
                    "/dev/romantics/Fotema/gtk/help-overlay.ui"
                )
                .object::<gtk::ShortcutsWindow>("help_overlay")
                .unwrap() -> gtk::ShortcutsWindow {
                    set_transient_for: Some(&main_window),
                    set_application: Some(&main_application()),
            },

            add_css_class?: if PROFILE == "Devel" {
                    Some("devel")
                } else {
                    None
                },

            add_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
                adw::BreakpointConditionLengthType::MaxWidth,
                500.0,
                adw::LengthUnit::Sp,
            )) {
                add_setter: (&header_bar, "show-title", &false.into()),
                add_setter: (&switcher_bar, "reveal", &true.into()),
                add_setter: (&main_navigation, "collapsed", &true.into()),
                //add_setter: (&main_navigation, "show-sidebar", &false.into()),
                add_setter: (&spinner, "visible", &true.into()),
            },

            // Top-level navigation view containing:
            // 1. Navigation view containing stack of pages.
            // 2. Page for displaying a single photo.
            #[local_ref]
            picture_navigation_view -> adw::NavigationView {
                set_pop_on_escape: true,
                connect_popped[sender] => move |_,_| sender.input(AppMsg::ViewHidden),

                // Page for showing main navigation. Such as "Library", "Selfies", etc.
                adw::NavigationPage {
                    set_title: "Main Navigation",

                    #[local_ref]
                    main_navigation -> adw::OverlaySplitView {

                        set_max_sidebar_width: 200.0,

                        #[wrap(Some)]
                        set_sidebar = &adw::NavigationPage {
                            adw::ToolbarView {
                                add_top_bar = &adw::HeaderBar {
                                    #[wrap(Some)]
                                    set_title_widget = &gtk::Label {
                                        set_label: "Photos",
                                        add_css_class: "title",
                                    },

                                    pack_end = &gtk::MenuButton {
                                        set_icon_name: "open-menu-symbolic",
                                        set_menu_model: Some(&primary_menu),
                                    }
                                },
                                #[wrap(Some)]
                                set_content = &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    gtk::StackSidebar {
                                        set_stack: &main_stack,
                                        set_vexpand: true,
                                    },
                                    #[local_ref]
                                    progress_box -> gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_margin_all: 12,
                                        set_visible: false,

                                        #[local_ref]
                                        progress_bar -> gtk::ProgressBar {
                                            set_show_text: true,
                                        },
                                    }
                                }
                            }
                        },

                        #[wrap(Some)]
                        set_content = &adw::NavigationPage {
                            set_title: "-",
                            adw::ToolbarView {
                                #[local_ref]
                                add_top_bar = &header_bar -> adw::HeaderBar {
                                    set_hexpand: true,
                                    pack_start = &gtk::Button {
                                        set_icon_name: "dock-left-symbolic",
                                        connect_clicked => AppMsg::ToggleSidebar,
                                    },

                                    //#[wrap(Some)]
                                    //set_title_widget = &adw::ViewSwitcher {
                                    //   set_stack: Some(model.library.widget()),
                                    //    set_policy: adw::ViewSwitcherPolicy::Wide,
                                    //},

                                    #[local_ref]
                                    pack_end = &spinner -> gtk::Spinner,
                                },

                                // NOTE I would like this to be an adw::ViewStack
                                // so that I could use a adw::ViewSwitcher in the sidebar
                                // that would show icons.
                                // However, adw::ViewSwitch can't display vertically.
                                #[wrap(Some)]
                                set_content = &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    #[local_ref]
                                    banner -> adw::Banner {
                                        // Only show when generating thumbnails
                                        set_button_label: None,
                                    },

                                    #[local_ref]
                                    main_stack -> gtk::Stack {
                                        connect_visible_child_notify => AppMsg::SwitchView,

                                        add_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            container_add: model.library.widget(),

                                            #[name(switcher_bar)]
                                            adw::ViewSwitcherBar {
                                                set_stack: Some(model.library.widget()),
                                            },
                                        } -> {
                                            set_title: "Library",
                                            set_name: ViewName::Library.into(),

                                            // NOTE gtk::StackSidebar doesn't show icon :-/
                                            set_icon_name: "image-alt-symbolic",
                                        },

                                        add_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            container_add: model.videos_page.widget(),
                                        } -> {
                                            set_title: "Videos",
                                            set_name: ViewName::Videos.into(),
                                            // NOTE gtk::StackSidebar doesn't show icon :-/
                                            set_icon_name: "video-reel-symbolic",
                                        },

                                        add_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            container_add: model.motion_page.widget(),
                                        } -> {
                                            set_title: "Animated",
                                            set_name: ViewName::Animated.into(),
                                            // NOTE gtk::StackSidebar doesn't show icon :-/
                                            set_icon_name: "sonar-symbolic",
                                        },

                                        add_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            container_add: model.selfies_page.widget(),
                                        } -> {
                                            set_visible: model.show_selfies,
                                            set_title: "Selfies",
                                            set_name: ViewName::Selfies.into(),
                                            // NOTE gtk::StackSidebar doesn't show icon :-/
                                            set_icon_name: "sentiment-very-satisfied-symbolic",
                                        },

                                        add_child = &adw::NavigationView {
                                            set_pop_on_escape: true,

                                            adw::NavigationPage {
                                                //set_tag: Some("folders"),
                                                //set_title: "Folder",
                                                model.folder_photos.widget(),
                                            },
                                        } -> {
                                            set_title: "Folders",
                                            set_name: ViewName::Folders.into(),
                                            // NOTE gtk::StackSidebar doesn't show icon :-/
                                            set_icon_name: "folder-symbolic",
                                        },
                                    },
                                },
                            },
                        },
                    },
                },

                adw::NavigationPage {
                    set_tag: Some("album"),
                    set_title: "-",
                    adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {
                            #[wrap(Some)]
                            set_title_widget = &gtk::Label {
                                set_label: "Folder", // TODO set title to folder name
                                add_css_class: "title",
                            }
                        },

                        #[wrap(Some)]
                        set_content = model.folder_album.widget(),
                    }
                },

                // Page for showing a single photo.
                adw::NavigationPage {
                    set_tag: Some("picture"),
                    set_title: "-",
                    model.one_photo.widget(),
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let data_dir = glib::user_data_dir().join(APP_ID);
        let _ = std::fs::create_dir_all(&data_dir);

        let cache_dir = glib::user_cache_dir().join(APP_ID);
        let _ = std::fs::create_dir_all(&cache_dir);

        let pic_base_dir = glib::user_special_dir(glib::enums::UserDirectory::Pictures)
            .expect("Expect XDG_PICTURES_DIR");

        let db_path = data_dir.join("pictures.sqlite");

        let con = database::setup(&db_path).expect("Must be able to open database");
        let con = Arc::new(Mutex::new(con));

        let video_transcoder = video::Transcoder::new(&cache_dir);

        let state = SharedState::new(relm4::SharedState::new());

        let bootstrap = Bootstrap::builder()
            .detach_worker((con.clone(), state.clone()))
            .forward(sender.input_sender(), |msg| match msg {
                BootstrapOutput::ProgressStarted(count, banner_msg, progress_label) => AppMsg::ProgressStarted(count, banner_msg, progress_label),
                BootstrapOutput::ProgressAdvanced => AppMsg::ProgressAdvanced,
                BootstrapOutput::ProgressCompleted => AppMsg::ProgressCompleted,
                BootstrapOutput::TaskStarted(msg) => AppMsg::TaskStarted(msg),
                BootstrapOutput::Completed => AppMsg::BootstrapCompleted,
            });

        let library = Library::builder()
            .launch(state.clone())
            .forward(sender.input_sender(), |msg| match msg {
                LibraryOutput::ViewPhoto(id) => AppMsg::ViewPhoto(id),
            });

        let photo_info = PhotoInfo::builder()
            .launch(state.clone())
            .detach();

        let one_photo = OnePhoto::builder()
            .launch((state.clone(), photo_info))
            .detach();

        let selfies_page = Album::builder()
            .launch((state.clone(), AlbumFilter::Selfies))
            .forward(sender.input_sender(), |msg| match msg {
                AlbumOutput::Selected(id) => AppMsg::ViewPhoto(id),
            });

        state.subscribe(selfies_page.sender(), |_| AlbumInput::Refresh);

        let show_selfies = AppWidgets::show_selfies();

        let motion_page = Album::builder()
            .launch((state.clone(), AlbumFilter::Motion))
            .forward(sender.input_sender(), |msg| match msg {
                AlbumOutput::Selected(id) => AppMsg::ViewPhoto(id),
            });

        state.subscribe(motion_page.sender(), |_| AlbumInput::Refresh);

        let videos_page = Album::builder()
            .launch((state.clone(), AlbumFilter::Videos))
            .forward(sender.input_sender(), |msg| match msg {
                AlbumOutput::Selected(id) => AppMsg::ViewPhoto(id),
            });

        state.subscribe(videos_page.sender(), |_| AlbumInput::Refresh);

        let folder_photos = FolderPhotos::builder()
            .launch(state.clone())
            .forward(
            sender.input_sender(),
            |msg| match msg {
                FolderPhotosOutput::FolderSelected(path) => AppMsg::ViewFolder(path),
            },
        );

        state.subscribe(folder_photos.sender(), |_| FolderPhotosInput::Refresh);

        let folder_album = Album::builder()
            .launch((state.clone(), AlbumFilter::None))
            .forward(sender.input_sender(), |msg| match msg {
                AlbumOutput::Selected(id) => AppMsg::ViewPhoto(id),
            });

        state.subscribe(folder_album.sender(), |_| AlbumInput::Refresh);

        let about_dialog = AboutDialog::builder().launch(root.clone()).detach();

        let preferences_dialog = PreferencesDialog::builder().launch(root.clone()).forward(
            sender.input_sender(),
            |msg| match msg {
                PreferencesOutput::Updated => AppMsg::PreferencesUpdated,
            },
        );

        let library_view_stack = adw::ViewStack::new();

        let picture_navigation_view = adw::NavigationView::builder().build();

        let main_navigation = adw::OverlaySplitView::builder().build();

        let main_stack = gtk::Stack::new();

        let header_bar = adw::HeaderBar::new();

        let spinner = gtk::Spinner::builder().visible(false).build();

        let progress_bar = gtk::ProgressBar::builder().pulse_step(0.05).build();

        let progress_box = gtk::Box::builder().build();

        let banner = adw::Banner::new("-");

        let model = Self {
            bootstrap,

            about_dialog,
            preferences_dialog,

            library,

            one_photo,
            motion_page,
            videos_page,
            selfies_page,
            show_selfies,
            folder_photos,
            folder_album,

            main_navigation: main_navigation.clone(),
            main_stack: main_stack.clone(),
            library_view_stack: library_view_stack.clone(),
            picture_navigation_view: picture_navigation_view.clone(),
            header_bar: header_bar.clone(),
            spinner: spinner.clone(),
            progress_bar: progress_bar.clone(),
            progress_box: progress_box.clone(),
            progress_end_count: 0,
            progress_current_count: 0,
            banner: banner.clone(),
        };

        let widgets = view_output!();

        let mut actions = RelmActionGroup::<WindowActionGroup>::new();

        let shortcuts_action = {
            let shortcuts = widgets.shortcuts.clone();
            RelmAction::<ShortcutsAction>::new_stateless(move |_| {
                shortcuts.present();
            })
        };

        let about_action = {
            let sender = model.about_dialog.sender().clone();
            RelmAction::<AboutAction>::new_stateless(move |_| {
                sender.send(()).unwrap();
            })
        };

        let preferences_action = {
            let sender = model.preferences_dialog.sender().clone();
            RelmAction::<PreferencesAction>::new_stateless(move |_| {
                sender.send(PreferencesInput::Present).unwrap();
            })
        };

        actions.add_action(shortcuts_action);
        actions.add_action(about_action);
        actions.add_action(preferences_action);

        actions.register_for_widget(&widgets.main_window);

        widgets.load_window_size();

        model.spinner.set_visible(true);
        model.spinner.start();

        model.bootstrap.emit(BootstrapInput::Start);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, _sender: ComponentSender<Self>) {
        match message {
            AppMsg::Quit => main_application().quit(),
            AppMsg::ToggleSidebar => {
                let show = self.main_navigation.shows_sidebar();
                self.main_navigation.set_show_sidebar(!show);
                self.spinner.set_visible(show);
            }
            AppMsg::SwitchView => {
                let child = self.main_stack.visible_child();
                let child_name = self.main_stack.visible_child_name()
                    .and_then(|x| ViewName::from_str(x.as_str()).ok())
                    .unwrap_or(ViewName::Nothing);

                // Set special library header, otherwise set standard label header
                if child_name == ViewName::Library {
                    let vs = adw::ViewSwitcher::builder()
                        .stack(self.library.widget())
                        .policy(adw::ViewSwitcherPolicy::Wide)
                        .build();
                    self.header_bar.set_title_widget(Some(&vs));
                } else if let Some(child) = child {
                    let page = self.main_stack.page(&child);
                    let title = page.title().map(|x| x.to_string());
                    let label = gtk::Label::builder()
                        .label(title.unwrap_or("-".to_string()))
                        .css_classes(["title"])
                        .build();
                    self.header_bar.set_title_widget(Some(&label));
                }

                // figure out which view to activate
                match child_name {
                    ViewName::Library  => self.library.emit(LibraryInput::Activate),
                    ViewName::Videos => self.videos_page.emit(AlbumInput::Activate),
                    ViewName::Selfies => self.selfies_page.emit(AlbumInput::Activate),
                    ViewName::Animated => self.motion_page.emit(AlbumInput::Activate),
                    ViewName::Folders => self.folder_photos.emit(FolderPhotosInput::Activate),
                    ViewName::Folder => self.folder_album.emit(AlbumInput::Activate),
                    ViewName::Nothing => println!("Nothing activated... which should not happen"),
                }
            }
            AppMsg::ViewPhoto(visual_id) => {
                // Send message to OnePhoto to show image
                self.one_photo.emit(OnePhotoInput::ViewPhoto(visual_id));

                // Display navigation page for viewing an individual photo.
                self.picture_navigation_view.push_by_tag("picture");
            }
            AppMsg::ViewHidden => {
                self.one_photo.emit(OnePhotoInput::Hidden);
            }
            AppMsg::ViewFolder(path) => {
                self.folder_album
                    .emit(AlbumInput::Filter(AlbumFilter::Folder(path)));
                //self.folder_album
                self.picture_navigation_view.push_by_tag("album");
            }
            AppMsg::TaskStarted(msg) => {
                self.spinner.start();
                self.banner.set_title(&msg);
                self.banner.set_revealed(true);
                self.progress_box.set_visible(false);
                self.progress_bar.set_text(None);
            }
            AppMsg::ProgressStarted(count, banner_title, progress_label) => {
                println!("Progress started: {}", banner_title);
                self.banner.set_title(&banner_title);
                self.banner.set_revealed(true);

                self.spinner.start();

                let show = self.main_navigation.shows_sidebar();
                self.spinner.set_visible(!show);

                self.progress_end_count = count;
                self.progress_current_count = 0;

                self.progress_box.set_visible(true);
                self.progress_bar.set_fraction(0.0);
                self.progress_bar.set_text(Some(&progress_label));
                self.progress_bar.set_pulse_step(0.25);
            }
            AppMsg::ProgressAdvanced => {
                println!("Progress advanced");
                self.progress_current_count += 1;

                // Show pulsing for first 20 items so that it catches the eye, then
                // switch to fractional view
                if self.progress_current_count < 20 {
                    self.progress_bar.pulse();
                } else {
                    if self.progress_current_count == 20 {
                        self.progress_bar.set_text(None);
                    }
                    let fraction =
                        self.progress_current_count as f64 / self.progress_end_count as f64;
                    self.progress_bar.set_fraction(fraction);
                }
            }
            AppMsg::ProgressCompleted => {
                println!("Progress completed.");
                self.spinner.stop();
                self.banner.set_revealed(false);
                self.progress_box.set_visible(false);
            }
            AppMsg::BootstrapCompleted => {
                println!("Bootstrap completed.");
                self.spinner.stop();
                self.banner.set_revealed(false);
                self.progress_bar.set_text(None);
                self.progress_box.set_visible(false);
            }
            AppMsg::PreferencesUpdated => {
                println!("Preferences updated.");
                // TODO create a Preferences struct to hold preferences and send with update message.
                self.show_selfies = AppWidgets::show_selfies();
            }
        }
    }

    fn shutdown(&mut self, widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        widgets.save_window_size().unwrap();
    }
}

impl AppWidgets {
    fn show_selfies() -> bool {
        let settings = gio::Settings::new(APP_ID);
        let show_selfies = settings.boolean("show-selfies");
        show_selfies
    }

    fn save_window_size(&self) -> Result<(), glib::BoolError> {
        let settings = gio::Settings::new(APP_ID);
        let (width, height) = self.main_window.default_size();

        settings.set_int("window-width", width)?;
        settings.set_int("window-height", height)?;

        settings.set_boolean("is-maximized", self.main_window.is_maximized())?;

        Ok(())
    }

    fn load_window_size(&self) {
        let settings = gio::Settings::new(APP_ID);

        let width = settings.int("window-width");
        let height = settings.int("window-height");
        let is_maximized = settings.boolean("is-maximized");

        self.main_window.set_default_size(width, height);

        if is_maximized {
            self.main_window.maximize();
        }
    }
}
