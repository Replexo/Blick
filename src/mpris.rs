// Copyright (c) 2026 Blick contributors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! MPRIS, so media keys and the desktop's media controls reach the shown
//! video
//!
//! The bus name is only owned while a video is shown, so the desktop does
//! not list Blick as a player while it shows photos.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;

use crate::deps::*;
use crate::widgets::LpVideoPlayer;

const BUS_NAME: &str = "org.mpris.MediaPlayer2.blick";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const TRACK_ID: &str = "/io/github/Replexo/Blick/Video";

const INTERFACES: &str = r#"
<node>
  <interface name="org.mpris.MediaPlayer2">
    <method name="Raise"/>
    <method name="Quit"/>
    <property name="CanQuit" type="b" access="read"/>
    <property name="CanRaise" type="b" access="read"/>
    <property name="HasTrackList" type="b" access="read"/>
    <property name="Identity" type="s" access="read"/>
    <property name="DesktopEntry" type="s" access="read"/>
    <property name="SupportedUriSchemes" type="as" access="read"/>
    <property name="SupportedMimeTypes" type="as" access="read"/>
  </interface>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="Next"/>
    <method name="Previous"/>
    <method name="Pause"/>
    <method name="PlayPause"/>
    <method name="Stop"/>
    <method name="Play"/>
    <method name="Seek">
      <arg direction="in" name="Offset" type="x"/>
    </method>
    <method name="SetPosition">
      <arg direction="in" name="TrackId" type="o"/>
      <arg direction="in" name="Position" type="x"/>
    </method>
    <method name="OpenUri">
      <arg direction="in" name="Uri" type="s"/>
    </method>
    <signal name="Seeked">
      <arg name="Position" type="x"/>
    </signal>
    <property name="PlaybackStatus" type="s" access="read"/>
    <property name="Rate" type="d" access="read"/>
    <property name="Metadata" type="a{sv}" access="read"/>
    <property name="Volume" type="d" access="read"/>
    <property name="Position" type="x" access="read"/>
    <property name="MinimumRate" type="d" access="read"/>
    <property name="MaximumRate" type="d" access="read"/>
    <property name="CanGoNext" type="b" access="read"/>
    <property name="CanGoPrevious" type="b" access="read"/>
    <property name="CanPlay" type="b" access="read"/>
    <property name="CanPause" type="b" access="read"/>
    <property name="CanSeek" type="b" access="read"/>
    <property name="CanControl" type="b" access="read"/>
  </interface>
</node>
"#;

thread_local! {
    /// The video that media keys control
    static PLAYER: RefCell<glib::WeakRef<LpVideoPlayer>> = RefCell::default();
    static OWNER: Cell<Option<gio::OwnerId>> = const { Cell::new(None) };
    static CONNECTION: RefCell<Option<gio::DBusConnection>> = const { RefCell::new(None) };
}

fn player() -> Option<LpVideoPlayer> {
    PLAYER.with_borrow(|weak| weak.upgrade())
}

/// `player` is now shown, or no video is when `None`
pub fn set_player(player: Option<&LpVideoPlayer>) {
    PLAYER.with_borrow_mut(|weak| weak.set(player));

    let owner = OWNER.take();
    match (player.is_some(), owner) {
        (true, None) => {
            let id = gio::bus_own_name(
                gio::BusType::Session,
                BUS_NAME,
                gio::BusNameOwnerFlags::NONE,
                register,
                |_, _| tracing::debug!("Owning {BUS_NAME}"),
                |_, _| tracing::debug!("Could not own {BUS_NAME}"),
            );
            OWNER.set(Some(id));
        }
        (false, Some(id)) => {
            // Releasing the name waits for the bus, so not on the main thread
            std::thread::spawn(move || gio::bus_unown_name(id));
            CONNECTION.take();
        }
        (_, owner) => OWNER.set(owner),
    }
    changed();
}

/// `player` is no longer shown; another video may already be
pub fn unset_player(player: &LpVideoPlayer) {
    if self::player().as_ref() == Some(player) {
        set_player(None);
    }
}

/// The player started, paused or moved on
pub fn changed() {
    let Some(connection) = CONNECTION.with_borrow(|c| c.clone()) else {
        return;
    };
    let properties = glib::VariantDict::new(None);
    for name in ["PlaybackStatus", "Metadata", "Volume", "Rate"] {
        properties.insert_value(name, &player_property(name));
    }
    let parameters = (
        PLAYER_INTERFACE,
        properties.end(),
        Vec::<String>::new(),
    )
        .to_variant();
    if let Err(err) = connection.emit_signal(
        None,
        OBJECT_PATH,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        Some(&parameters),
    ) {
        tracing::debug!("Could not send MPRIS changes: {err}");
    }
}

fn register(connection: gio::DBusConnection, _name: &str) {
    let node = match gio::DBusNodeInfo::for_xml(INTERFACES) {
        Ok(node) => node,
        Err(err) => {
            tracing::error!("Invalid MPRIS interfaces: {err}");
            return;
        }
    };
    for interface in node.interfaces() {
        let result = connection
            .register_object(OBJECT_PATH, interface)
            .method_call(|_, _, _, interface, method, parameters, invocation| {
                call(interface.unwrap_or_default(), method, &parameters);
                invocation.return_value(None);
            })
            .property(|_, _, _, interface, name| {
                if interface == PLAYER_INTERFACE {
                    player_property(name)
                } else {
                    root_property(name)
                }
            })
            .build();
        if let Err(err) = result {
            tracing::warn!("Could not register MPRIS: {err}");
        }
    }
    CONNECTION.set(Some(connection));
}

fn call(interface: &str, method: &str, parameters: &glib::Variant) {
    let Some(player) = player() else {
        return;
    };
    let microseconds = |us: i64| gst::ClockTime::from_useconds(us.max(0) as u64);
    match (interface, method) {
        (_, "Raise") => player.present(),
        (_, "Quit") => {}
        (_, "Play") => player.set_playing(true),
        (_, "Pause") => player.set_playing(false),
        (_, "PlayPause") => player.toggle_playing(),
        (_, "Stop") => {
            player.set_playing(false);
            player.seek_to(gst::ClockTime::ZERO);
        }
        (_, "Next") => player.navigate(true),
        (_, "Previous") => player.navigate(false),
        (_, "Seek") => {
            if let (Some((offset,)), Some(position)) =
                (parameters.get::<(i64,)>(), player.position())
            {
                let target = position.useconds() as i64 + offset;
                player.seek_to(microseconds(target));
            }
        }
        (_, "SetPosition") => {
            if let Some((_, position)) = parameters.get::<(glib::variant::ObjectPath, i64)>() {
                player.seek_to(microseconds(position));
            }
        }
        _ => {}
    }
}

fn root_property(name: &str) -> glib::Variant {
    match name {
        "CanQuit" | "HasTrackList" => false.to_variant(),
        "CanRaise" => true.to_variant(),
        "Identity" => "Blick".to_variant(),
        "DesktopEntry" => crate::config::APP_ID.to_variant(),
        "SupportedUriSchemes" => vec!["file"].to_variant(),
        "SupportedMimeTypes" => Vec::<String>::new().to_variant(),
        _ => false.to_variant(),
    }
}

fn player_property(name: &str) -> glib::Variant {
    let player = player();
    match name {
        "PlaybackStatus" => match &player {
            Some(player) if player.is_playing() => "Playing",
            Some(_) => "Paused",
            None => "Stopped",
        }
        .to_variant(),
        "Rate" => player.map_or(1., |p| p.rate()).to_variant(),
        "MinimumRate" => 0.5.to_variant(),
        "MaximumRate" => 2.0.to_variant(),
        "Volume" => crate::widgets::video_player::volume().to_variant(),
        "Position" => player
            .and_then(|p| p.position())
            .map_or(0, |p| p.useconds() as i64)
            .to_variant(),
        "Metadata" => metadata(player.as_ref()),
        "CanGoNext" | "CanGoPrevious" | "CanPlay" | "CanPause" | "CanSeek" | "CanControl" => {
            player.is_some().to_variant()
        }
        _ => false.to_variant(),
    }
}

fn metadata(player: Option<&LpVideoPlayer>) -> glib::Variant {
    let dict = glib::VariantDict::new(None);
    if let Some(player) = player {
        if let Ok(path) = glib::variant::ObjectPath::try_from(TRACK_ID) {
            dict.insert_value("mpris:trackid", &path.to_variant());
        }
        if let Some(file) = player.file() {
            if let Some(name) = file.basename() {
                dict.insert_value("xesam:title", &name.display().to_string().to_variant());
            }
            dict.insert_value("xesam:url", &file.uri().to_string().to_variant());
        }
        if let Some(duration) = player.duration() {
            dict.insert_value("mpris:length", &(duration.useconds() as i64).to_variant());
        }
    }
    dict.end()
}
