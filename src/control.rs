//! Remote control of commissioned lights: the OnOff, LevelControl and ColorControl clusters, invoked over CASE.

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::color_control::{self, ColorControlClient, ColorModeEnum};
use rs_matter::dm::clusters::decl::descriptor::DescriptorAttrReads;
use rs_matter::dm::clusters::decl::level_control::{self, LevelControlClient, OptionsBitmap};
use rs_matter::dm::clusters::decl::on_off::{self, OnOffClient};
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::dm::EndptId;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::im::client::{ImClient, ReadSender};
use rs_matter::im::{AttrResp, ReadReqBuilder};
use rs_matter::persist::KvBlobStoreAccess;
use rs_matter::tlv::{FromTLV, Nullable, TLVArray};
use rs_matter::transport::exchange::Exchange;
use serde::{Deserialize, Serialize};

use crate::controller::Controller;

/// The endpoint of a node that hosts its light.
#[derive(Debug, Clone, Copy)]
pub struct Light {
    pub endpoint: EndptId,
    /// Whether the endpoint has the LevelControl cluster.
    pub dimmable: bool,
    /// Whether the endpoint has the ColorControl cluster.
    pub color: bool,
    /// Whether its ColorControl takes CIE xy colors (the `XY` feature); hue/saturation otherwise.
    pub xy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LightState {
    pub node_id: u64,
    pub endpoint: EndptId,
    pub on: bool,
    /// Brightness, 1-254; `None` for lights that cannot be dimmed.
    pub level: Option<u8>,
    /// Current color as `#rrggbb` at full brightness; `None` for lights without color.
    pub color: Option<String>,
}

/// A change to a light; fields left out stay as they are.
#[derive(Debug, Clone, Deserialize)]
pub struct LightChange {
    pub on: Option<bool>,
    /// Brightness, 1-254. Setting it also turns the light on, unless `on` is `false`.
    pub level: Option<u8>,
    /// Color as `#rrggbb`; only its hue and saturation are used, brightness is `level`.
    pub color: Option<String>,
    /// Seconds over which the light fades to the new `level` and `color` (0.1 s steps).
    pub transition: Option<f32>,
}

impl LightChange {
    /// `transition` in Matter's tenths of a second, or `None` if it is out of range.
    pub fn transition_time(&self) -> Option<u16> {
        let tenths = (self.transition.unwrap_or(0.0) * 10.0).round();
        (0.0..=u16::MAX as f32)
            .contains(&tenths)
            .then_some(tenths as u16)
    }
}

/// `#rrggbb` of red, green and blue in 0-1.
pub fn rgb_hex(r: f32, g: f32, b: f32) -> String {
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// `#rrggbb` of Matter's hue (0-254 for 0-360°) and saturation (0-254), at full brightness.
pub fn hue_saturation_hex(hue: u8, saturation: u8) -> String {
    let h = hue as f32 / 254.0 * 6.0;
    let s = saturation as f32 / 254.0;
    let x = 1.0 - (h.rem_euclid(2.0) - 1.0).abs();
    let (r, g, b) = match h as u8 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    let tint = |c: f32| 1.0 - s * (1.0 - c);
    rgb_hex(tint(r), tint(g), tint(b))
}

/// `#rrggbb` of a CIE 1931 chromaticity (Matter's `CurrentX`/`CurrentY`, 0-65279 for 0-1), at full brightness.
pub fn xy_hex(x: u16, y: u16) -> String {
    let (x, y) = (x as f32 / 65536.0, (y as f32 / 65536.0).max(f32::EPSILON));
    let (cx, cz) = (x / y, (1.0 - x - y) / y);
    let linear = [
        3.2406 * cx - 1.5372 - 0.4986 * cz,
        -0.9689 * cx + 1.8758 + 0.0415 * cz,
        0.0557 * cx - 0.2040 + 1.0570 * cz,
    ]
    .map(|c| c.max(0.0));
    let max = linear.iter().cloned().fold(f32::EPSILON, f32::max);
    let [r, g, b] = linear.map(|c| {
        let c = c / max;
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    });
    rgb_hex(r, g, b)
}

/// `#rrggbb` of a white color temperature in mireds (Tanner Helland's approximation).
pub fn mireds_hex(mireds: u16) -> String {
    let t = 10_000.0 / mireds.max(1) as f32;
    let r = if t <= 66.0 {
        255.0
    } else {
        329.7 * (t - 60.0).powf(-0.1332)
    };
    let g = if t <= 66.0 {
        99.47 * t.ln() - 161.12
    } else {
        288.12 * (t - 60.0).powf(-0.0755)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.52 * (t - 10.0).ln() - 305.04
    };
    rgb_hex(r / 255.0, g / 255.0, b / 255.0)
}

/// Matter's hue (0-254 for 0-360°) and saturation (0-254) of a `#rrggbb` color.
pub fn hue_saturation([r, g, b]: [f32; 3]) -> (u8, u8) {
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    let degrees = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if max == 0.0 { 0.0 } else { delta / max };

    (
        (degrees / 360.0 * 254.0).round() as u8,
        (saturation * 254.0).round() as u8,
    )
}

/// Red, green and blue in 0-1 of a `#rrggbb` color.
pub fn rgb(color: &str) -> Option<[f32; 3]> {
    let hex = color.strip_prefix('#').filter(|hex| hex.len() == 6)?;
    let channel = |i: usize| {
        u8::from_str_radix(&hex[i..i + 2], 16)
            .ok()
            .map(|c| c as f32 / 255.0)
    };
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// Matter's `CurrentX`/`CurrentY` (0-65279 for 0-1) of an sRGB color: its CIE 1931 chromaticity.
/// Black has none and maps to the D65 white point.
pub fn xy(rgb: [f32; 3]) -> (u16, u16) {
    let [r, g, b] = rgb.map(|c| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    });
    let cx = 0.4124 * r + 0.3576 * g + 0.1805 * b;
    let cy = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cz = 0.0193 * r + 0.1192 * g + 0.9505 * b;
    let sum = cx + cy + cz;
    let (x, y) = if sum > 0.0 {
        (cx / sum, cy / sum)
    } else {
        (0.3127, 0.3290)
    };
    let value = |c: f32| (c * 65536.0).round().min(65279.0) as u16;
    (value(x), value(y))
}

impl<'a, C: Crypto, K: KvBlobStoreAccess> Controller<'a, C, K> {
    /// A new exchange with a commissioned node, over its CASE session (established on first use).
    pub async fn exchange(&self, node_id: u64) -> Result<Exchange<'a>, Error> {
        Exchange::initiate(self.matter, &self.crypto, self.fab_idx, node_id).await
    }

    /// Read a list attribute, built by `request`.
    ///
    /// Devices may send a list as an empty list followed by one report per appended
    /// item, which rs-matter's generated `*_read_with` readers do not reassemble.
    pub async fn read_list<T>(
        &self,
        node_id: u64,
        request: impl FnMut(ReadReqBuilder<ReadSender<'a>>) -> Result<ReadSender<'a>, Error>,
    ) -> Result<Vec<T>, Error>
    where
        T: for<'t> FromTLV<'t>,
    {
        let mut items = Vec::new();
        let mut chunk = self.exchange(node_id).await?.read_with(request).await?;

        loop {
            for report in chunk.response()?.attr_reports.iter().flatten() {
                match report? {
                    // An appended item: its path has a (null) list index.
                    AttrResp::Data(data) if data.path.list_index.is_some() => {
                        items.push(T::from_tlv(&data.data)?)
                    }
                    AttrResp::Data(data) => {
                        items = TLVArray::<T>::new(data.data)?
                            .iter()
                            .collect::<Result<_, _>>()?
                    }
                    AttrResp::Status(status) => {
                        return Err(status
                            .status
                            .status
                            .to_error_code()
                            .unwrap_or(ErrorCode::Failure)
                            .into())
                    }
                }
            }

            match chunk.complete().await? {
                Some(next) => chunk = next,
                None => return Ok(items),
            }
        }
    }

    /// The first endpoint of `node_id` with the OnOff cluster.
    pub async fn light(&self, node_id: u64) -> Result<Light, Error> {
        if let Some(light) = self.lights.borrow().get(&node_id) {
            return Ok(*light);
        }

        let parts: Vec<EndptId> = self
            .read_list(node_id, |msg| {
                msg.attr_requests()?
                    .descriptor_read()
                    .parts_list(ROOT_ENDPOINT_ID)?
                    .end()?
                    .fabric_filtered(true)?
                    .end()
            })
            .await?;

        for endpoint in parts {
            let servers: Vec<u32> = self
                .read_list(node_id, |msg| {
                    msg.attr_requests()?
                        .descriptor_read()
                        .server_list(endpoint)?
                        .end()?
                        .fabric_filtered(true)?
                        .end()
                })
                .await?;

            if servers.contains(&on_off::FULL_CLUSTER.id) {
                let color = servers.contains(&color_control::FULL_CLUSTER.id);
                let xy = color
                    && self
                        .exchange(node_id)
                        .await?
                        .color_control()
                        .feature_map_read(endpoint)
                        .await?
                        & color_control::Feature::XY.bits()
                        != 0;
                let light = Light {
                    endpoint,
                    dimmable: servers.contains(&level_control::FULL_CLUSTER.id),
                    color,
                    xy,
                };
                self.lights.borrow_mut().insert(node_id, light);
                return Ok(light);
            }
        }

        Err(ErrorCode::NotFound.into())
    }

    pub async fn light_state(&self, node_id: u64) -> Result<LightState, Error> {
        let light = self.light(node_id).await?;
        self.read_light(node_id, light).await
    }

    /// Apply `change`, without reading the light's state back (one round trip per command).
    pub async fn change_light(&self, node_id: u64, change: &LightChange) -> Result<(), Error> {
        let light = self.light(node_id).await?;
        let transition = change.transition_time().ok_or(ErrorCode::InvalidArgument)?;

        if let Some(level) = change.level {
            if !light.dimmable {
                return Err(ErrorCode::InvalidCommand.into());
            }
            self.exchange(node_id)
                .await?
                .level_control()
                .move_to_level_with_on_off(light.endpoint, |req| {
                    req.level(level)?
                        .transition_time(Nullable::some(transition))?
                        .options_mask(OptionsBitmap::empty())?
                        .options_override(OptionsBitmap::empty())?
                        .end()
                })
                .await?;
        }

        match change.on {
            Some(true) => {
                self.exchange(node_id)
                    .await?
                    .on_off()
                    .on(light.endpoint)
                    .await?
            }
            Some(false) => {
                self.exchange(node_id)
                    .await?
                    .on_off()
                    .off(light.endpoint)
                    .await?
            }
            None => {}
        }

        if let Some(color) = &change.color {
            let color = rgb(color).ok_or(ErrorCode::InvalidArgument)?;
            if !light.color {
                return Err(ErrorCode::InvalidCommand.into());
            }
            // Applied even while the light is off, so it comes on in that color.
            let if_off = color_control::OptionsBitmap::EXECUTE_IF_OFF;
            let exchange = self.exchange(node_id).await?;
            if light.xy {
                // Device-independent: the light maps the chromaticity onto its own LEDs.
                let (x, y) = xy(color);
                exchange
                    .color_control()
                    .move_to_color(light.endpoint, |req| {
                        req.color_x(x)?
                            .color_y(y)?
                            .transition_time(transition)?
                            .options_mask(if_off)?
                            .options_override(if_off)?
                            .end()
                    })
                    .await?;
            } else {
                let (hue, saturation) = hue_saturation(color);
                exchange
                    .color_control()
                    .move_to_hue_and_saturation(light.endpoint, |req| {
                        req.hue(hue)?
                            .saturation(saturation)?
                            .transition_time(transition)?
                            .options_mask(if_off)?
                            .options_override(if_off)?
                            .end()
                    })
                    .await?;
            }
        }

        Ok(())
    }

    pub async fn toggle_light(&self, node_id: u64) -> Result<(), Error> {
        let light = self.light(node_id).await?;
        self.exchange(node_id)
            .await?
            .on_off()
            .toggle(light.endpoint)
            .await
    }

    pub async fn read_light(&self, node_id: u64, light: Light) -> Result<LightState, Error> {
        let on = self
            .exchange(node_id)
            .await?
            .on_off()
            .on_off_read(light.endpoint)
            .await?;
        let level = if light.dimmable {
            self.exchange(node_id)
                .await?
                .level_control()
                .current_level_read(light.endpoint)
                .await?
                .into_option()
        } else {
            None
        };
        let color = if light.color {
            Some(self.read_color(node_id, light.endpoint).await?)
        } else {
            None
        };

        Ok(LightState {
            node_id,
            endpoint: light.endpoint,
            on,
            level,
            color,
        })
    }

    /// The current color in whichever mode the light is in.
    pub async fn read_color(&self, node_id: u64, endpoint: EndptId) -> Result<String, Error> {
        let mode = self
            .exchange(node_id)
            .await?
            .color_control()
            .color_mode_read(endpoint)
            .await?;

        Ok(match mode {
            ColorModeEnum::CurrentHueAndCurrentSaturation => {
                let hue = self
                    .exchange(node_id)
                    .await?
                    .color_control()
                    .current_hue_read(endpoint)
                    .await?;
                let saturation = self
                    .exchange(node_id)
                    .await?
                    .color_control()
                    .current_saturation_read(endpoint)
                    .await?;
                hue_saturation_hex(hue, saturation)
            }
            ColorModeEnum::CurrentXAndCurrentY => {
                let x = self
                    .exchange(node_id)
                    .await?
                    .color_control()
                    .current_x_read(endpoint)
                    .await?;
                let y = self
                    .exchange(node_id)
                    .await?
                    .color_control()
                    .current_y_read(endpoint)
                    .await?;
                xy_hex(x, y)
            }
            ColorModeEnum::ColorTemperatureMireds => mireds_hex(
                self.exchange(node_id)
                    .await?
                    .color_control()
                    .color_temperature_mireds_read(endpoint)
                    .await?,
            ),
        })
    }
}
