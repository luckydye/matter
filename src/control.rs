//! Remote control of commissioned lights: the OnOff and LevelControl clusters, invoked over CASE.

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::color_control;
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
}

#[derive(Debug, Clone, Serialize)]
pub struct LightState {
    pub node_id: u64,
    pub endpoint: EndptId,
    pub on: bool,
    /// Brightness, 1-254; `None` for lights that cannot be dimmed.
    pub level: Option<u8>,
    /// Whether the light supports color (the ColorControl cluster).
    pub color: bool,
}

/// A change to a light; fields left out stay as they are.
#[derive(Debug, Clone, Deserialize)]
pub struct LightChange {
    pub on: Option<bool>,
    /// Brightness, 1-254. Setting it also turns the light on, unless `on` is `false`.
    pub level: Option<u8>,
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
                return Ok(Light {
                    endpoint,
                    dimmable: servers.contains(&level_control::FULL_CLUSTER.id),
                    color: servers.contains(&color_control::FULL_CLUSTER.id),
                });
            }
        }

        Err(ErrorCode::NotFound.into())
    }

    pub async fn light_state(&self, node_id: u64) -> Result<LightState, Error> {
        let light = self.light(node_id).await?;
        self.read_light(node_id, light).await
    }

    pub async fn change_light(
        &self,
        node_id: u64,
        change: &LightChange,
    ) -> Result<LightState, Error> {
        let light = self.light(node_id).await?;

        if let Some(level) = change.level {
            if !light.dimmable {
                return Err(ErrorCode::InvalidCommand.into());
            }
            self.exchange(node_id)
                .await?
                .level_control()
                .move_to_level_with_on_off(light.endpoint, |req| {
                    req.level(level)?
                        .transition_time(Nullable::none())?
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

        self.read_light(node_id, light).await
    }

    pub async fn toggle_light(&self, node_id: u64) -> Result<LightState, Error> {
        let light = self.light(node_id).await?;
        self.exchange(node_id)
            .await?
            .on_off()
            .toggle(light.endpoint)
            .await?;

        self.read_light(node_id, light).await
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

        Ok(LightState {
            node_id,
            endpoint: light.endpoint,
            on,
            level,
            color: light.color,
        })
    }
}
