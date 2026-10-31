/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Utilities for working with bundles. So far we are only interested in the
//! application bundle.
//!
//! Relevant Apple documentation:
//! * [Bundle Programming Guide](https://developer.apple.com/library/archive/documentation/CoreFoundation/Conceptual/CFBundles/Introduction/Introduction.html)
//!   * [Anatomy of an iOS Application Bundle](https://developer.apple.com/library/archive/documentation/CoreFoundation/Conceptual/CFBundles/BundleTypes/BundleTypes.html)
//! * [Bundle Resources](https://developer.apple.com/documentation/bundleresources?language=objc)

use crate::fs::{BundleData, Fs, GuestPath, GuestPathBuf};
use crate::image::Image;
use crate::window::{DeviceFamily, DeviceOrientation};
use plist::dictionary::Dictionary;
use plist::Value;
use std::io::Cursor;

#[derive(Debug)]
pub struct Bundle {
    path: GuestPathBuf,
    plist: Dictionary,
}

impl Bundle {
    /// See [Fs::new] for meaning of `read_only_mode`.
    pub fn new_bundle_and_fs_from_host_path(
        mut bundle_data: BundleData,
        read_only_mode: bool,
    ) -> Result<(Bundle, Fs), String> {
        let plist_bytes = bundle_data.read_plist()?;

        let plist = Value::from_reader(Cursor::new(plist_bytes))
            .map_err(|_| "Could not deserialize plist data".to_string())?;

        let plist = plist
            .into_dictionary()
            .ok_or_else(|| "plist root value is not a dictionary".to_string())?;

        let bundle_name = format!(
            "{}.app",
            if let Some(canonical) = plist.get("CFBundleName") {
                canonical.as_string().unwrap()
            } else {
                bundle_data.bundle_name()
            }
        );
        let bundle_id = plist["CFBundleIdentifier"].as_string().unwrap();

        let (fs, guest_path) = Fs::new(bundle_data, bundle_name, bundle_id, read_only_mode);

        let bundle = Bundle {
            path: guest_path,
            plist,
        };

        Ok((bundle, fs))
    }

    /// Create a fake bundle (see [crate::Environment::new_without_app]).
    pub fn new_fake_bundle() -> Bundle {
        Bundle {
            path: GuestPathBuf::from(String::new()),
            plist: Dictionary::new(),
        }
    }

    pub fn bundle_path(&self) -> &GuestPath {
        &self.path
    }

    pub fn bundle_identifier(&self) -> &str {
        self.plist
            .get("CFBundleIdentifier")
            .and_then(|v| v.as_string())
            .unwrap_or("org.touchhle.app-picker")
    }

    pub fn bundle_version(&self) -> &str {
        self.plist
            .get("CFBundleVersion")
            .or_else(|| self.plist.get("CFBundleShortVersionString"))
            .and_then(|v| v.as_string())
            .unwrap_or("0")
    }

    pub fn bundle_localizations(&self) -> &[Value] {
        static EMPTY_VAL: Vec<Value> = Vec::new();
        self.plist
            .get("CFBundleLocalizations")
            .and_then(|v| v.as_array())
            .unwrap_or(&EMPTY_VAL)
    }

    /// Canonical name for the bundle according to Info.plist
    pub fn canonical_bundle_name(&self) -> Option<&str> {
        self.plist
            .get("CFBundleName")
            .map(|name| name.as_string().unwrap())
    }

    /// Name for the bundle, either the canonical name or, if there isn't one,
    /// the name this bundle has in the filesystem.
    pub fn bundle_name(&self) -> &str {
        self.path.file_name().unwrap().strip_suffix(".app").unwrap()
    }

    pub fn display_name(&self) -> &str {
        if let Some(display_name) = self.plist.get("CFBundleDisplayName") {
            display_name.as_string().unwrap()
        } else {
            ""
        }
    }

    pub fn minimum_os_version(&self) -> Option<String> {
        // `MinimumOSVersion` is documented as a string (e.g. "3.0"), but some
        // bundles — particularly 64-bit App Store builds — encode it as a
        // real number (3.0) or integer (3) in their binary Info.plist.
        // `as_string().unwrap()` panicked on those, taking down the whole
        // emulator. Accept any of those encodings and normalise to a string.
        self.plist.get("MinimumOSVersion").and_then(|v| {
            if let Some(s) = v.as_string() {
                Some(s.to_string())
            } else if let Some(i) = v.as_signed_integer() {
                Some(i.to_string())
            } else if let Some(u) = v.as_unsigned_integer() {
                Some(u.to_string())
            } else {
                v.as_real().map(|r| {
                    // Format e.g. 3.0 as "3", 6.1 as "6.1" — the consumer in
                    // lib.rs parses major/minor numerically and tolerates a
                    // missing minor component.
                    if r.fract() == 0.0 {
                        format!("{}", r as i64)
                    } else {
                        format!("{}", r)
                    }
                })
            }
        })
    }

    pub fn required_device_capabilities(&self) -> Vec<&str> {
        self.plist
            .get("UIRequiredDeviceCapabilities")
            .map(|v| {
                if let Some(dict) = v.as_dictionary() {
                    // TODO: support undesired capabilities
                    assert!(dict.values().all(|x| x.as_boolean().unwrap()));
                    dict.keys().map(|o| o.as_str()).collect()
                } else {
                    v.as_array()
                        .unwrap()
                        .iter()
                        .map(|o| o.as_string().unwrap())
                        .collect()
                }
            })
            .unwrap_or_default()
    }

    pub fn executable_path(&self) -> GuestPathBuf {
        // FIXME: Is this key optional? All iPhone apps seem to have it.
        self.path
            .join(self.plist["CFBundleExecutable"].as_string().unwrap())
    }

    pub fn launch_image_path(&self, fs: &Fs, device_family: DeviceFamily) -> GuestPathBuf {
        // Check if there's a custom base name in plist
        let base_name = self
            .plist
            .get("UILaunchImageFile")
            .map(|v| v.as_string().unwrap())
            .unwrap_or("Default");

        // Try device-specific variants first, then fallback to base name
        let candidates = if device_family.is_ipad() {
            if device_family.is_retina() {
                vec![
                    format!("{}@2x~ipad.png", base_name), // iPad Retina
                    format!("{}~ipad.png", base_name),    // iPad non-Retina
                    format!("{}@2x.png", base_name),      // Fallback Retina
                    format!("{}.png", base_name),         // Fallback
                ]
            } else {
                vec![
                    format!("{}~ipad.png", base_name),    // iPad non-Retina
                    format!("{}@2x~ipad.png", base_name), // iPad Retina
                    format!("{}.png", base_name),         // Fallback
                ]
            }
        } else if device_family.is_phone_568() {
            vec![
                format!("{}-568h@2x.png", base_name), // iPhone 5 (4-inch)
                format!("{}@2x.png", base_name),      // iPhone Retina
                format!("{}.png", base_name),         // iPhone non-Retina
            ]
        } else if device_family.is_retina() {
            vec![
                format!("{}@2x.png", base_name), // iPhone Retina
                format!("{}.png", base_name),    // iPhone non-Retina
            ]
        } else {
            vec![
                format!("{}@2x.png", base_name), // iPhone Retina (fallback)
                format!("{}.png", base_name),    // iPhone non-Retina
            ]
        };

        // Find the first existing file
        for candidate in &candidates {
            let path = self.path.join(candidate);
            if fs.read(&path).is_ok() {
                log!("Using launch image: {}", candidate);
                return path;
            }
        }

        // Final fallback
        log!("Warning: No launch image found, using Default.png");
        self.path.join("Default.png")
    }

    pub fn status_bar_hidden(&self) -> bool {
        self.plist
            .get("UIStatusBarHidden")
            .and_then(|v| v.as_boolean())
            .unwrap_or(false)
    }

    /// Resolve all candidate icon files declared by the bundle's
    /// Info.plist, in priority order. Returns paths from most-specific
    /// to least-specific so callers can fall back when a file is
    /// missing on disk.
    ///
    /// Apple's documented lookup order (see "App Icons on iPhone, iPod
    /// touch, and iPad", `CFBundleIcons` reference, and historical
    /// `CFBundleIconFile`):
    ///
    /// 1. `CFBundleIcons` (iOS 5+) → `CFBundlePrimaryIcon` →
    ///    `CFBundleIconFiles[]` (an array of stem names; iOS picks the
    ///    one whose suffix matches the device class — `@2x.png`,
    ///    `@2x~ipad.png`, …; we just try the literal name and the
    ///    explicit "@2x.png"/".png" decorations).
    /// 2. `CFBundleIconFile` (legacy single-string key, iOS 2/3/4).
    /// 3. Hard-coded defaults: `Icon.png`, `icon.png`,
    ///    `Icon@2x.png`.
    fn icon_path_candidates(&self) -> Vec<GuestPathBuf> {
        let mut candidates: Vec<String> = Vec::new();

        let push = |candidates: &mut Vec<String>, name: &str| {
            let lower = name.to_lowercase();
            if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
                candidates.push(name.to_string());
            } else {
                candidates.push(format!("{name}.png"));
                candidates.push(format!("{name}@2x.png"));
                candidates.push(format!("{name}.jpg"));
            }
        };

        // 1. CFBundleIcons → CFBundlePrimaryIcon → CFBundleIconFiles (iOS 5+).
        if let Some(icons) = self.plist.get("CFBundleIcons") {
            if let Some(dict) = icons.as_dictionary() {
                if let Some(primary) = dict.get("CFBundlePrimaryIcon") {
                    if let Some(primary_dict) = primary.as_dictionary() {
                        if let Some(files) = primary_dict.get("CFBundleIconFiles") {
                            if let Some(arr) = files.as_array() {
                                for entry in arr {
                                    if let Some(s) = entry.as_string() {
                                        push(&mut candidates, s);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // The iPad-specific variant (`CFBundleIcons~ipad`) lives at the
        // top level too.
        if let Some(icons) = self.plist.get("CFBundleIcons~ipad") {
            if let Some(dict) = icons.as_dictionary() {
                if let Some(primary) = dict.get("CFBundlePrimaryIcon") {
                    if let Some(primary_dict) = primary.as_dictionary() {
                        if let Some(files) = primary_dict.get("CFBundleIconFiles") {
                            if let Some(arr) = files.as_array() {
                                for entry in arr {
                                    if let Some(s) = entry.as_string() {
                                        push(&mut candidates, s);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. CFBundleIconFiles[] at the top level (rare; iOS 3.2+ aux key).
        if let Some(files) = self.plist.get("CFBundleIconFiles") {
            if let Some(arr) = files.as_array() {
                for entry in arr {
                    if let Some(s) = entry.as_string() {
                        push(&mut candidates, s);
                    }
                }
            }
        }

        // 3. Legacy CFBundleIconFile (iOS 2/3/4).
        if let Some(filename) = self.plist.get("CFBundleIconFile") {
            if let Some(s) = filename.as_string() {
                push(&mut candidates, s);
            }
        }

        // 4. Hard-coded defaults documented in
        //    "Icon Files" of the iPhone Application Programming Guide.
        for default in [
            "Icon.png",
            "icon.png",
            "Icon@2x.png",
            "Icon-72.png",
            "Icon-Small.png",
        ] {
            candidates.push(default.to_string());
        }

        // Deduplicate while preserving order.
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.clone()));

        candidates
            .into_iter()
            .map(|name| self.path.join(name))
            .collect()
    }

    fn icon_path(&self) -> GuestPathBuf {
        // Backwards-compatible single-path accessor: return the first
        // candidate produced by the full lookup. The new `load_icon`
        // path tries all of them in order before giving up.
        self.icon_path_candidates()
            .into_iter()
            .next()
            .unwrap_or_else(|| self.path.join("Icon.png"))
    }

    /// Load icon and round off its corners (and add sheen if needed) for
    /// display.
    pub fn load_icon(&self, fs: &Fs) -> Result<Image, String> {
        let candidates = self.icon_path_candidates();
        let mut last_err: Option<String> = None;
        let bytes = candidates.iter().find_map(|path| match fs.read(path) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                last_err = Some(format!("missing: {}", path.as_str()));
                None
            }
        });
        let bytes = bytes.ok_or_else(|| {
            // Mirror the historical phrasing so any tooling that scrapes
            // the warning still recognises it.
            "Could not read icon file".to_string()
        })?;
        let _ = last_err;
        let mut image =
            Image::from_bytes(&bytes).map_err(|e| format!("Could not parse icon image: {e}"))?;
        // UIPrerenderedIcon is used to avoid iOS applying a sheen effect,
        // should be boolean, but some apps use a string, so we check both.
        // See https://developer.apple.com/library/archive/qa/qa1614/_index.html
        // Default if it does not exist is NO/false.
        let add_sheen = !self
            .plist
            .get("UIPrerenderedIcon")
            .and_then(|v| v.as_boolean().or(v.as_string().map(|s| s == "YES")))
            .unwrap_or(false);
        // iPhone OS icons are 57px by 57px and the OS always applies a
        // 10px radius rounded corner (see e.g. documentation of
        // UIPrerenderedIcon). If the icon is larger for some reason,
        // let's scale to match.
        let corner_radius = (10.0 / 57.0) * (image.dimensions().0 as f32);
        image.round_corners(corner_radius, /* four_corners: */ true, add_sheen);
        Ok(image)
    }

    pub fn main_nib_filename(&self, device_family: Option<DeviceFamily>) -> Option<&str> {
        // TODO: extend this logic for all device-specific keys
        if let Some(device_family) = device_family {
            if device_family.is_ipad() && self.plist.get("NSMainNibFile~ipad").is_some()
            {
                return self
                    .plist
                    .get("NSMainNibFile~ipad")
                    .map(|v| v.as_string().unwrap());
            }
        }
        self.plist
            .get("NSMainNibFile")
            .map(|v| v.as_string().unwrap())
    }

    /// The value of `UIMainStoryboardFile` (or its `~ipad` device-specific
    /// variant) from `Info.plist`, if any. This is the modern (iOS 5+)
    /// replacement for `NSMainNibFile`: the value is the base name (without
    /// extension) of a compiled storyboard found in the bundle resources
    /// (e.g. `Main` resolves to `Base.lproj/Main~iphone.storyboardc/` on
    /// iPhone).
    pub fn main_storyboard_filename(
        &self,
        device_family: Option<DeviceFamily>,
    ) -> Option<&str> {
        if let Some(device_family) = device_family {
            if device_family.is_ipad()
                && self.plist.get("UIMainStoryboardFile~ipad").is_some()
            {
                return self
                    .plist
                    .get("UIMainStoryboardFile~ipad")
                    .map(|v| v.as_string().unwrap());
            }
        }
        self.plist
            .get("UIMainStoryboardFile")
            .map(|v| v.as_string().unwrap())
    }

    /// Whether the app declares its orientation at all. Pre-3.2 apps often
    /// declare none and rotate themselves in code (Crash Bandicoot Nitro Kart
    /// 2 does), so [Self::supported_interface_orientations]'s portrait default
    /// says nothing about how they render.
    pub fn declares_interface_orientation(&self) -> bool {
        self.plist.contains_key("UISupportedInterfaceOrientations")
            || self.plist.contains_key("UIInterfaceOrientation")
    }

    pub fn supported_interface_orientations(&self) -> Vec<&str> {
        // Apple's Bundle Resources documentation
        // (https://developer.apple.com/documentation/bundleresources/information-property-list/uisupportedinterfaceorientations)
        // says UISupportedInterfaceOrientations (iOS 3.2+) is an Array of
        // Strings, and UIInterfaceOrientation (iPhone OS 2.0+) is a single
        // String. UISupportedInterfaceOrientations takes precedence.
        //
        // In practice some pre-3.2 apps (e.g. Tiny Zoo Friends, plist
        // observed in the wild) store UISupportedInterfaceOrientations as
        // a single String — sometimes even a comma-separated list — which
        // is technically malformed but iOS accepts it. We mirror that
        // tolerance instead of crashing the host emulator.
        if let Some(v) = self.plist.get("UISupportedInterfaceOrientations") {
            if let Some(arr) = v.as_array() {
                return arr
                    .iter()
                    .filter_map(|o| o.as_string())
                    .collect();
            }
            if let Some(s) = v.as_string() {
                if s.contains(',') {
                    log!(
                        "UISupportedInterfaceOrientations is a comma-separated string ({}); splitting.",
                        s
                    );
                }
                return s.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
            }
            log!(
                "UISupportedInterfaceOrientations has unexpected plist type {:?}; ignoring.",
                v
            );
        }

        if let Some(v) = self.plist.get("UIInterfaceOrientation") {
            let str = v.as_string().unwrap_or("UIInterfaceOrientationPortrait");
            if str.contains(',') {
                log!("UIInterfaceOrientation is a comma separated list of strings ({}), splitting!", str);
            }
            return str.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
        }

        vec!["UIInterfaceOrientationPortrait"]
    }

    pub fn device_family_array(&self) -> Vec<DeviceFamily> {
        // Apple docs: UIDeviceFamily is an Array of Numbers (1=iPhone, 2=iPad).
        // https://developer.apple.com/documentation/bundleresources/information-property-list/uidevicefamily
        // We additionally accept strings ("1", "2", "iphone", "ipad") because
        // a few real-world plists serialise the values that way; ignore
        // unknown entries instead of panicking the emulator.
        let Some(v) = self.plist.get("UIDeviceFamily") else {
            return vec![DeviceFamily::iPhone];
        };
        let Some(arr) = v.as_array() else {
            log!(
                "UIDeviceFamily has unexpected plist type {:?}; defaulting to iPhone.",
                v
            );
            return vec![DeviceFamily::iPhone];
        };
        let mut families = Vec::new();
        for o in arr {
            let parsed = match o {
                Value::Integer(i) => i.as_unsigned().and_then(|n| DeviceFamily::try_from(n).ok()),
                Value::String(s) => {
                    // First try numeric; fall back to symbolic name.
                    s.parse::<u64>()
                        .ok()
                        .and_then(|n| DeviceFamily::try_from(n).ok())
                        .or_else(|| DeviceFamily::try_from(s.as_str()).ok())
                }
                _ => None,
            };
            match parsed {
                Some(family) => families.push(family),
                None => log!(
                    "UIDeviceFamily entry {:?} is not a recognised device family; ignoring.",
                    o
                ),
            }
        }
        if families.is_empty() {
            vec![DeviceFamily::iPhone]
        } else {
            families
        }
    }
}
