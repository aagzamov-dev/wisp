use super::*;

impl Settings {
    pub(super) fn notch(&self, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut displays = self.topbar_displays.clone();
        for id in &self.config.topbar_monitors {
            if id != "all" && id != "primary" && !displays.iter().any(|(known, _, _)| known == id) {
                displays.push((id.clone(), format!("{id} (disconnected)"), false));
            }
        }
        v_flex().gap_2().child(label("Top bar",p)).child(group(p)
            .child(row("Enable top bar","Move tray and system controls to the top; keep Start and apps in the dock.",
                Switch::new("topbar").checked(self.config.topbar).accessibility_label("Enable top bar")
                    .on_change(cx.listener(|this,enabled:&bool,window,cx|{this.save("topbar",(*enabled).into(),window,cx);})),p))
            .child(divider(p))
            .child(item("Height",format!("{} px at 100% Windows scaling; 24–48 px",self.config.topbar_height),p)
                .child(Slider::new(&self.topbar_height).accessibility_label("Top bar height")))
            .child(divider(p))
            .child(item("Bar transparency",format!("{}% transparent. Icons keep their contrast.",100-self.config.topbar_opacity),p)
                .child(Slider::new(&self.topbar_transparency).accessibility_label("Top bar transparency")))
            .child(divider(p))
            .child(row("Blur behind island", "Optional Windows 11 glass. Windows transparency and power settings can disable it; the island keeps its tint when unavailable.",
                Switch::new("topbar-island-blur").checked(self.config.topbar_island_blur).accessibility_label("Blur behind island")
                    .on_change(cx.listener(|this,enabled:&bool,window,cx| {this.save("topbar_island_blur",(*enabled).into(),window,cx);})),p))
            .child(divider(p))
            .child(item("Island opacity",format!("{}%. Dark island tint; rounded corners stay transparent.",self.config.topbar_island_opacity),p)
                .child(Slider::new(&self.topbar_island_opacity).accessibility_label("Island opacity"))))
            .child(div().h_3()).child(label("Displays",p))
            .child(group(p).child(item("Show on","Choose the main display, every display, or toggle individual displays below. Refresh this page after connecting a display.",p)
                .child(h_flex().gap_2()
                    .child(Button::new("topbar-primary").label("Main display").when(self.config.topbar_monitors==["primary"],|b|b.primary())
                        .on_click(cx.listener(|this,_,window,cx|{let array:Array=["primary"].into_iter().collect();this.save("topbar_monitors",Value::Array(array),window,cx);})))
                    .child(Button::new("topbar-all").label("All displays").when(self.config.topbar_monitors==["all"],|b|b.primary())
                        .on_click(cx.listener(|this,_,window,cx|{let array:Array=["all"].into_iter().collect();this.save("topbar_monitors",Value::Array(array),window,cx);})))
                    .child(Button::new("topbar-refresh").label("Refresh displays").on_click(cx.listener(|this,_,_,cx|{this.topbar_displays=system::topbar_displays();cx.notify();})))))
                .children(displays.into_iter().enumerate().map(|(index,(id,name,primary))|{
                    let checked=self.config.topbar_monitors.contains(&id) || self.config.topbar_monitors==["all"] || self.config.topbar_monitors==["primary"] && primary;
                    row(name,"Show the top bar on this display",
                        Switch::new(("topbar-monitor",index)).checked(checked).accessibility_label(format!("Show top bar on {id}"))
                            .on_change(cx.listener(move|this,enabled:&bool,window,cx|{
                                let mut ids=if this.config.topbar_monitors==["all"]{this.topbar_displays.iter().map(|(id,_,_)|id.clone()).collect()}else if this.config.topbar_monitors==["primary"]{this.topbar_displays.iter().filter(|(_,_,primary)|*primary).map(|(id,_,_)|id.clone()).collect()}else{this.config.topbar_monitors.clone()};
                                ids.retain(|known|known!=&id);if *enabled{ids.push(id.clone());}
                                let array:Array=ids.into_iter().collect();this.save("topbar_monitors",Value::Array(array),window,cx);
                            })),p)
                })))
            .child(hint("Tray: hold Ctrl and drag an icon between the bar and the overflow menu. Press Esc to cancel.",p))
            .child(div().h_3()).child(self.music_sources(p,cx))
            .child(div().h_3()).child(label("Standalone music notch",p)).child(row(
            "Music notch",
            "Used when the top bar is off. Show a player on the main display and click to expand it.",
            Switch::new("music-notch")
                .checked(self.config.music_notch)
                .accessibility_label("Music notch")
                .on_change(cx.listener(|this, enabled: &bool, window, cx| {
                    this.save("music_notch", (*enabled).into(), window, cx);
                })),
            p,
        ))
        .into_any_element()
    }
    fn music_sources(&self, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let status = if self.media_loading {
            "Checking Windows media sources..."
        } else if !self.media_loaded {
            "Refresh to find available media apps"
        } else if self.media_error.is_some() && self.media_sources.is_empty() {
            "Could not read Windows media sessions"
        } else if self.media_error.is_some() {
            "Previous sources remain available below"
        } else if self.media_sources.is_empty() {
            "No media apps found. Start playback in an app, then refresh."
        } else {
            "Apps with available Windows media sessions"
        };
        let source_rows = self
            .media_sources
            .iter()
            .map(|source| {
                // A friendly picker label must not broaden runtime exclusions.
                let excluded = crate::media_identity::excluded(
                    &source.id,
                    &crate::media_identity::app_name(&source.id),
                    &self.config.music_excluded_apps,
                );
                let id = source.id.clone();
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(source.name.clone()))
                            .child(div().text_xs().text_color(p.muted).truncate().child(source.id.clone())),
                    )
                    .child(
                        Button::new(SharedString::from(format!("exclude-media-source:{}", source.id)))
                            .label(if excluded { "Excluded" } else { "Exclude" })
                            .disabled(excluded || self.media_loading)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let values =
                                    crate::media_identity::add_exact(&this.config.music_excluded_apps, [id.clone()]);
                                let array: Array = values.into_iter().collect();
                                this.save("music_excluded_apps", Value::Array(array), window, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();
        let available = group(p)
            .child(row(
                "Active media apps",
                status,
                Button::new("refresh-media-sources")
                    .label("Refresh")
                    .disabled(self.media_loading)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_media_sources(cx))),
                p,
            ))
            .children(self.media_error.as_ref().map(|error| error_text(error, p)))
            .when(!source_rows.is_empty(), |group| {
                group.child(
                    div()
                        .id("media-source-list")
                        .max_h(px(248.))
                        .overflow_y_scroll()
                        .child(v_flex().gap_1().children(source_rows)),
                )
            });
        let presets = item(
            "Quick exclusions",
            "Browser exclusions hide all media from that browser",
            p,
        )
        .child(h_flex().flex_wrap().gap_2().children(
            ["Google Chrome", "Microsoft Edge", "Firefox"].into_iter().map(|name| {
                Button::new(name)
                    .label(format!("Exclude {name}"))
                    .disabled(name == "Firefox" && self.media_loading)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let additions = if name == "Firefox" {
                            crate::media_identity::firefox_additions(&this.media_firefox)
                        } else {
                            vec![name.into()]
                        };
                        let values = crate::media_identity::add_exact(&this.config.music_excluded_apps, additions);
                        let array: Array = values.into_iter().collect();
                        this.save("music_excluded_apps", Value::Array(array), window, cx);
                    }))
            }),
        ));
        let manual =
            item(
                "Add an app manually",
                "Use its complete app name or Windows media source ID",
                p,
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Input::new(&self.media_app).aria_label("Media app to exclude").flex_1())
                    .child(Button::new("exclude-media-app").label("Add").on_click(cx.listener(
                        |this, _, window, cx| {
                            let name = this.media_app.read(cx).value().trim().to_string();
                            if !name.is_empty() {
                                let values = crate::media_identity::add_exact(&this.config.music_excluded_apps, [name]);
                                let array: Array = values.into_iter().collect();
                                if this.save("music_excluded_apps", Value::Array(array), window, cx) {
                                    this.media_app.update(cx, |input, cx| input.set_value("", window, cx));
                                }
                            }
                        },
                    ))),
            );
        let saved = self
            .config
            .music_excluded_apps
            .iter()
            .enumerate()
            .map(|(index, name)| {
                row(
                    name.clone(),
                    "Excluded source",
                    Button::new(("remove-media-app", index))
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let mut values = this.config.music_excluded_apps.clone();
                            if index < values.len() {
                                values.remove(index);
                            }
                            let array: Array = values.into_iter().collect();
                            this.save("music_excluded_apps", Value::Array(array), window, cx);
                        })),
                    p,
                )
            })
            .collect::<Vec<_>>();
        v_flex().gap_2().child(label("Music sources",p)).child(available)
            .child(group(p).child(presets).child(divider(p)).child(manual).children(saved))
            .child(hint("Only app identities are read. Firefox uses locally registered installation IDs when available; otherwise select its exact source above. Portable or unregistered apps may show only an ID. Windows does not provide reliable URLs, so site-only exclusions remain unavailable.",p))
            .into_any_element()
    }
}
