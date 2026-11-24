use super::*;
use crate::dock_gradient::{Direction, Gradient};

const BACKGROUNDS: [(&str, &str); 2] = [("shared-material", "Shared material"), ("gradient", "Gradient")];
const DIRECTIONS: [(&str, &str); 3] = [
    ("left-to-right", "Left to right"),
    ("top-to-bottom", "Top to bottom"),
    ("diagonal", "Diagonal"),
];
const PRESETS: [(&str, Gradient); 4] = [
    (
        "Midnight Plum",
        Gradient::preset([0x20, 0x29, 0x43], [0x38, 0x2B, 0x45]),
    ),
    ("Deep Teal", Gradient::preset([0x12, 0x34, 0x3A], [0x23, 0x3F, 0x4D])),
    ("Ember", Gradient::preset([0x30, 0x25, 0x39], [0x5B, 0x36, 0x32])),
    ("Slate", Gradient::preset([0x24, 0x2A, 0x34], [0x3D, 0x46, 0x56])),
];

type SavedThemes = SelectState<SearchableVec<String>>;

pub(super) struct Controls {
    background: Entity<Choices>,
    direction: Entity<Choices>,
    colors: [Entity<ColorPickerState>; 2],
    dragging_color: [bool; 2],
    transparency: Entity<SliderState>,
    themes: Entity<SavedThemes>,
    theme_names: Vec<String>,
    theme_name: Entity<InputState>,
}

impl Controls {
    pub(super) fn new(config: &Config, window: &mut Window, cx: &mut Context<Settings>) -> Self {
        let background = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(labels(&BACKGROUNDS)),
                index_of(&BACKGROUNDS, &config.dock_background),
                window,
                cx,
            )
        });
        let gradient = config.dock_gradient;
        let direction = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(labels(&DIRECTIONS)),
                index_of(&DIRECTIONS, config::direction_key(gradient.direction)),
                window,
                cx,
            )
        });
        let colors = [gradient.start, gradient.end]
            .map(|value| cx.new(|cx| ColorPickerState::new(window, cx).default_value(swatch(value))));
        let transparency = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value(gradient.transparency as f32)
        });
        let theme_names = theme_names(config);
        let themes =
            cx.new(|cx| SelectState::new(SearchableVec::new(theme_names.clone()), theme_index(config), window, cx));
        let theme_name = cx.new(|cx| InputState::new(window, cx).placeholder("Theme name"));

        cx.subscribe_in(&background, window, |this, _, event, window, cx| {
            if let SelectEvent::Confirm(Some(label)) = event
                && let Some(value) = value_of(&BACKGROUNDS, label)
            {
                this.save("dock_background", value.into(), window, cx);
                this.dock_gradient.sync(&this.config, window, cx);
            }
        })
        .detach();
        cx.subscribe_in(&direction, window, |this, _, event, window, cx| {
            if let SelectEvent::Confirm(Some(label)) = event
                && let Some(value) = value_of(&DIRECTIONS, label).and_then(Direction::parse)
                && value != this.config.dock_gradient.direction
            {
                this.update_gradient(|gradient| gradient.direction = value, window, cx);
            }
        })
        .detach();
        for (index, color) in colors.iter().enumerate() {
            let sliders = color.read(cx).sliders().clone();
            for component in [
                sliders.hue(),
                sliders.saturation(),
                sliders.lightness(),
                sliders.alpha(),
            ] {
                cx.subscribe_in(component, window, move |this, _, event, window, cx| match event {
                    SliderEvent::Change(_) => this.dock_gradient.dragging_color[index] = true,
                    SliderEvent::Release(_) => {
                        this.dock_gradient.dragging_color[index] = false;
                        if let Some(color) = this.dock_gradient.colors[index].read(cx).value() {
                            this.commit_gradient_color(index, color, window, cx);
                        }
                    }
                })
                .detach();
            }
            cx.subscribe_in(color, window, move |this, _, event, window, cx| {
                // The picker queues Change from its component slider's event. Its release
                // handler above commits the settled color, while discrete edits save here.
                if let ColorPickerEvent::Change(Some(color)) = event {
                    if this.dock_gradient.dragging_color[index] {
                        cx.notify();
                    } else {
                        this.commit_gradient_color(index, *color, window, cx);
                    }
                }
            })
            .detach();
        }
        cx.subscribe_in(&transparency, window, |this, _, event, window, cx| match event {
            SliderEvent::Change(_) => cx.notify(),
            SliderEvent::Release(value) => {
                this.commit_gradient_transparency(value.start().round() as u32, window, cx);
            }
        })
        .detach();
        cx.subscribe_in(&themes, window, |this, _, event, window, cx| {
            if let SelectEvent::Confirm(Some(name)) = event {
                this.save_with("dock_gradient", |store| store.apply_gradient_theme(name), window, cx);
                this.dock_gradient.sync(&this.config, window, cx);
            }
        })
        .detach();
        cx.subscribe_in(&theme_name, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.save_gradient_theme(window, cx);
            }
        })
        .detach();

        Self {
            background,
            direction,
            colors,
            dragging_color: [false; 2],
            transparency,
            themes,
            theme_names,
            theme_name,
        }
    }

    pub(super) fn sync(&mut self, config: &Config, window: &mut Window, cx: &mut Context<Settings>) {
        let gradient = config.dock_gradient;
        for (select, index) in [
            (&self.background, index_of(&BACKGROUNDS, &config.dock_background)),
            (
                &self.direction,
                index_of(&DIRECTIONS, config::direction_key(gradient.direction)),
            ),
        ] {
            if select.read(cx).selected_index(cx) != index {
                select.update(cx, |state, cx| state.set_selected_index(index, window, cx));
            }
        }
        for (field, color) in self.colors.iter().zip([gradient.start, gradient.end]) {
            // Rewriting the picker's full-precision HSLA value would interrupt its sliders.
            if field.read(cx).value().map(rgb_channels) != Some(color) {
                field.update(cx, |state, cx| state.set_value(swatch(color), window, cx));
            }
        }
        let transparency = gradient.transparency as f32;
        if self.transparency.read(cx).value().start() != transparency {
            self.transparency
                .update(cx, |state, cx| state.set_value(transparency, window, cx));
        }
        if self
            .theme_names
            .iter()
            .ne(config.dock_gradient_themes.iter().map(|(name, _)| name))
        {
            self.theme_names = theme_names(config);
            self.themes.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(self.theme_names.clone()), window, cx);
            });
        }
        let index = theme_index(config);
        if self.themes.read(cx).selected_index(cx) != index {
            self.themes
                .update(cx, |state, cx| state.set_selected_index(index, window, cx));
        }
    }

    pub(super) fn background_row(&self, p: Palette) -> Div {
        row(
            "Dock background",
            "Choose a gradient or use the shared material.",
            Select::new(&self.background)
                .accessibility_label("Dock background")
                .w(px(160.)),
            p,
        )
    }

    pub(super) fn section(&self, config: &Config, p: Palette, cx: &mut Context<Settings>) -> Div {
        let gradient = config.dock_gradient;
        let [start, end] = self
            .colors
            .each_ref()
            .map(|picker| picker.read(cx).value().map(rgb_channels));
        let preview_gradient = Gradient {
            start: start.unwrap_or(gradient.start),
            end: end.unwrap_or(gradient.end),
            transparency: self.transparency.read(cx).value().start().round() as u32,
            ..gradient
        };
        let presets = PRESETS.into_iter().enumerate().map(|(index, (name, preset))| {
            let selected = gradient == preset;
            Button::new(("dock-gradient-preset", index))
                .label(name)
                .selected(selected)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_gradient(|gradient| *gradient = preset, window, cx);
                }))
        });
        let colors = ["Start color", "End color"]
            .into_iter()
            .zip(&self.colors)
            .map(|(label, state)| row(label, "", ColorPicker::new(state).accessibility_label(label), p));
        v_flex().gap_2().child(label("Dock gradient", p)).child(
            group(p)
                .child(div().px_4().py_3().child(preview(preview_gradient, p)))
                .child(divider(p))
                .child(
                    item("Presets", "Choose a starting palette.", p)
                        .child(h_flex().flex_wrap().gap_2().children(presets)),
                )
                .child(divider(p))
                .children(colors)
                .child(divider(p))
                .child(row(
                    "Direction",
                    "",
                    Select::new(&self.direction)
                        .accessibility_label("Gradient direction")
                        .w(px(160.)),
                    p,
                ))
                .child(divider(p))
                .child(
                    item(
                        "Transparency",
                        format!(
                            "{}% · 0% opaque, 100% clear. Applies to both colors.",
                            preview_gradient.transparency
                        ),
                        p,
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("dock-gradient-transparency-minus")
                                    .label("−")
                                    .accessibility_label("Decrease dock gradient transparency by one percent")
                                    .disabled(preview_gradient.transparency == 0)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let value = this.config.dock_gradient.transparency.saturating_sub(1);
                                        this.commit_gradient_transparency(value, window, cx);
                                    })),
                            )
                            .child(
                                Slider::new(&self.transparency)
                                    .accessibility_label("Dock gradient transparency")
                                    .flex_1(),
                            )
                            .child(
                                Button::new("dock-gradient-transparency-plus")
                                    .label("+")
                                    .accessibility_label("Increase dock gradient transparency by one percent")
                                    .disabled(preview_gradient.transparency == 100)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let value = (this.config.dock_gradient.transparency + 1).min(100);
                                        this.commit_gradient_transparency(value, window, cx);
                                    })),
                            ),
                    ),
                )
                .when(!config.dock_gradient_themes.is_empty(), |group| {
                    group.child(divider(p)).child(row(
                        "Saved themes",
                        "",
                        Select::new(&self.themes)
                            .placeholder("Choose a theme")
                            .accessibility_label("Saved dock gradient themes")
                            .w(px(160.)),
                        p,
                    ))
                })
                .child(divider(p))
                .child(
                    item("Save a theme", "", p).child(
                        h_flex()
                            .gap_2()
                            .child(
                                Input::new(&self.theme_name)
                                    .aria_label("Dock gradient theme name")
                                    .flex_1(),
                            )
                            .child(
                                Button::new("save-dock-gradient-theme")
                                    .label("Save theme")
                                    .on_click(cx.listener(|this, _, window, cx| this.save_gradient_theme(window, cx))),
                            ),
                    ),
                )
                .child(divider(p))
                .child(row(
                    "Reset gradient",
                    "Restore the default colors, direction and transparency.",
                    Button::new("reset-dock-gradient")
                        .label("Reset")
                        .disabled(gradient == Gradient::default())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.update_gradient(|gradient| *gradient = Gradient::default(), window, cx);
                        })),
                    p,
                )),
        )
    }
}

impl Settings {
    fn commit_gradient_color(&mut self, index: usize, color: Hsla, window: &mut Window, cx: &mut Context<Self>) {
        let value = rgb_channels(color);
        if color.a != 1. {
            self.dock_gradient.colors[index].update(cx, |state, cx| {
                state.set_value(color.alpha(1.), window, cx);
            });
        }
        let current = self.config.dock_gradient;
        if value != [current.start, current.end][index] {
            self.update_gradient(
                |gradient| {
                    if index == 0 {
                        gradient.start = value;
                    } else {
                        gradient.end = value;
                    }
                },
                window,
                cx,
            );
        }
    }

    fn commit_gradient_transparency(&mut self, value: u32, window: &mut Window, cx: &mut Context<Self>) {
        if value != self.config.dock_gradient.transparency {
            self.update_gradient(|gradient| gradient.transparency = value, window, cx);
        }
    }

    fn update_gradient(&mut self, edit: impl FnOnce(&mut Gradient), window: &mut Window, cx: &mut Context<Self>) {
        self.save_with(
            "dock_gradient",
            |store| {
                store.update_gradient(|mut gradient| {
                    edit(&mut gradient);
                    gradient
                })
            },
            window,
            cx,
        );
        self.dock_gradient.sync(&self.config, window, cx);
    }

    fn save_gradient_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.dock_gradient.theme_name.read(cx).value();
        if self.save_with(
            "dock_gradient_themes",
            |store| store.save_gradient_theme(&name),
            window,
            cx,
        ) {
            self.dock_gradient
                .theme_name
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.dock_gradient.sync(&self.config, window, cx);
        }
    }
}

fn theme_index(config: &Config) -> Option<IndexPath> {
    config
        .dock_gradient_themes
        .iter()
        .position(|(_, theme)| *theme == config.dock_gradient)
        .map(IndexPath::new)
}

fn theme_names(config: &Config) -> Vec<String> {
    config
        .dock_gradient_themes
        .iter()
        .map(|(name, _)| name.clone())
        .collect()
}

fn rgb_channels(color: Hsla) -> [u8; 3] {
    let rgb = color.to_rgb();
    [rgb.r, rgb.g, rgb.b].map(|value| (value.clamp(0., 1.) * 255.).round() as u8)
}

fn swatch(color: [u8; 3]) -> Hsla {
    let [r, g, b] = color;
    rgb(u32::from_be_bytes([0, r, g, b])).into()
}

fn preview(gradient: Gradient, p: Palette) -> Div {
    let fill = canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let opacity = 1. - gradient.transparency as f32 / 100.;
            let start = swatch(gradient.start).opacity(opacity);
            let end = swatch(gradient.end).opacity(opacity);
            let (angle, start_stop, end_stop) = match gradient.direction {
                Direction::LeftToRight => (90., 0., 1.),
                Direction::TopToBottom => (180., 0., 1.),
                Direction::Diagonal => {
                    let width = f32::from(bounds.size.width);
                    let height = f32::from(bounds.size.height);
                    if width <= 0. || height <= 0. {
                        return;
                    }
                    // GPUI scales the short component down by the aspect ratio.
                    // Undo that scaling and place the stops at the two corners so
                    // normalized x and y contribute equally, as on the dock.
                    let angle = 90. + width.powi(2).atan2(height.powi(2)).to_degrees();
                    let span = width.max(height) / width.hypot(height);
                    (angle, 0.5 - span, 0.5 + span)
                }
            };
            let fill = linear_gradient(
                angle,
                linear_color_stop(start, start_stop),
                linear_color_stop(end, end_stop),
            );
            window.paint_quad(quad(
                bounds,
                px(7.),
                fill,
                px(0.),
                transparent_black(),
                BorderStyle::default(),
            ));
        },
    )
    .h_12()
    .w_full();
    v_flex().gap_2().child(hint("Preview", p)).child(
        div()
            .rounded_lg()
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .child(fill),
    )
}
