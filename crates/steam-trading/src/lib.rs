//! Steam trade manager is the module that allows you to automate trade offers, by extending `SteamAuthenticator`.
//!
//! It inherently needs `SteamAuthenticator` as a dependency, since we need cookies from Steam Community and Steam Store
//! to be able to create and accept trade offers, along with mobile confirmations.
//!
//! **IT IS VERY IMPORTANT THAT STEAM GUARD IS ENABLED ON THE ACCOUNT BEING USED, WITH MOBILE CONFIRMATIONS.**
//!
//! Currently, `SteamAuthenticator` is "stateless", in comparison of alternatives such as Node.js.
//! This means that it does not need to stay up running and react to events.
//!
//! But this also means that you will need to keep track of trades and polling yourself, but it won't be much work,
//! since there are convenience functions for almost every need.
//!
//! Perhaps the event based trading experience will be an extension someday, but for now this works fine.
//!
//! Compiles on stable Rust.

#![allow(dead_code)]
// #![warn(missing_docs, missing_doc_code_examples)]
#![deny(
    missing_debug_implementations,
    missing_copy_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unsafe_code,
    unused_import_braces,
    unused_qualifications
)]

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;

use const_format::concatcp;
pub use errors::{OfferError, TradeError, TradelinkError};
use futures::stream::FuturesOrdered;
use futures::{StreamExt, TryFutureExt};
use futures_timer::Delay;
use serde::de::DeserializeOwned;
use steam_language_gen::generated::enums::ETradeOfferState;
use steam_mobile::client::SteamAuthenticator;
use steam_mobile::{ConfirmationMethod, Confirmations, HeaderMap, Method, STEAM_COMMUNITY_HOST};
use steamid_parser::SteamID;
use tappet::response_types::{GetTradeHistoryResponse, GetTradeOffersResponse, TradeHistory_Trade, TradeOffer_Trade};
use tappet::{Executor, ExecutorResponse, SteamAPI};
use tracing::{debug, info};
pub use types::asset_collection::AssetCollection;
pub use types::trade_link::Tradelink;
pub use types::trade_offer::TradeOffer;

use crate::additional_checks::check_steam_guard_error;
use crate::api_extensions::{FilterBy, HasAssets};
use crate::errors::TradeError::PayloadError;
use crate::errors::{error_from_strmessage, tradeoffer_error_from_eresult, ConfirmationError};
use crate::types::sessionid::HasSessionID;
use crate::types::trade_offer_web::{
    TradeOfferAcceptRequest, TradeOfferCancelResponse, TradeOfferCommonParameters, TradeOfferCreateRequest,
    TradeOfferCreateResponse, TradeOfferGenericErrorResponse, TradeOfferGenericRequest, TradeOfferParams,
};
use crate::types::TradeKind;

mod additional_checks;
pub mod api_extensions;
mod errors;
#[cfg(feature = "time")]
pub mod time;
mod types;

const TRADEOFFER_BASE: &str = "https://steamcommunity.com/tradeoffer/";
const TRADEOFFER_NEW_URL: &str = concatcp!(TRADEOFFER_BASE, "new/send");

/// This is decided upon various factors, mainly stability of Steam servers when dealing with huge
/// trade offers.
///
/// Consider this when creating trade websites.
pub const TRADE_MAX_ITEMS: u8 = u8::MAX;

/// Max trade offers to a single account.
pub const TRADE_MAX_TRADES_PER_SINGLE_USER: u8 = 5;

/// Max total sent trade offers.
pub const TRADE_MAX_ONGOING_TRADES: u8 = 30;

/// Standard delay, in milliseconds
const STANDARD_DELAY: u64 = 1000;

const MAX_HISTORICAL_CUTOFF: u32 = u32::MAX;

#[derive(Debug)]
pub struct SteamTradeManager<'a> {
    authenticator: &'a SteamAuthenticator,
    api_client: Rc<RefCell<Option<SteamAPI>>>,
}

impl<'a> SteamTradeManager<'a> {
    pub fn new(authenticator: &'a SteamAuthenticator) -> SteamTradeManager<'a> {
        Self {
            authenticator: &authenticator,
            api_client: Rc::new(RefCell::new(None)),
        }
    }

    /// SteamAPI only gets created if API methods are needed.
    /// Returns a reference to the `api_client`.
    fn lazy_web_api_client<T: ToString>(&self, api_key: T) -> &Rc<RefCell<Option<SteamAPI>>> {
        {
            let mut api_client = self.api_client.borrow_mut();

            match *api_client {
                Some(_) => {}
                None => *api_client = Some(SteamAPI::new(api_key.to_string())),
            };
        }

        &self.api_client
    }

    /// Checks whether the user of `tradelink` has recently activated his mobile SteamGuard.
    pub async fn check_steam_guard_recently_activated(&self, tradelink: Tradelink) -> Result<(), TradeError> {
        let Tradelink { partner_id, token, .. } = tradelink;

        check_steam_guard_error(self.authenticator, partner_id, &*token).await
    }

    /// Call to GetTradeOffers endpoint.
    ///
    /// Convenience function that fetches information about active trades for the current logged in account.
    pub async fn get_trade_offers(
        &self,
        sent: bool,
        received: bool,
        active_only: bool,
    ) -> Result<GetTradeOffersResponse, TradeError> {
        let api_key = self
            .authenticator
            .api_key()
            .expect("Api should be cached for this method to work.");
        let api_client = self.lazy_web_api_client(api_key).borrow();

        api_client
            .as_ref()
            .unwrap()
            .get()
            .IEconService()
            .GetTradeOffers(
                sent,
                received,
                MAX_HISTORICAL_CUTOFF,
                Some(active_only),
                None,
                None,
                None,
            )
            .execute_with_response()
            .err_into()
            .await
    }

    /// Call to GetTradeHistory endpoint.
    /// If not set, defaults to a max of 500 trade offers.
    ///
    /// Information about completed trades, and recover new asset ids.
    async fn get_trade_offers_history(
        &self,
        max_trades: Option<u32>,
        include_failed: bool,
    ) -> Result<GetTradeHistoryResponse, TradeError> {
        let api_key = self
            .authenticator
            .api_key()
            .expect("API key must be cached in order to use this.");
        let max_trades = max_trades.unwrap_or(500);
        let api_client = self.lazy_web_api_client(api_key).borrow();

        api_client
            .as_ref()
            .unwrap()
            .get()
            .IEconService()
            .GetTradeHistory(max_trades, include_failed, false, None, None, None, None, None)
            .execute_with_response()
            .err_into()
            .await
    }

    /// Returns a single raw trade offer by its id.
    pub async fn get_tradeoffer_by_id(&self, tradeoffer_id: i64) -> Result<Vec<TradeOffer_Trade>, TradeError> {
        self.get_trade_offers(true, true, true)
            .map_ok(|tradeoffers| tradeoffers.filter_by(|offer| offer.tradeofferid == tradeoffer_id))
            .await
    }

    pub async fn get_new_assetids(&self, tradeid: i64) -> Result<Vec<i64>, TradeError> {
        let found_trade: TradeHistory_Trade = self
            .get_trade_offers_history(None, false)
            .map_ok(|tradeoffers| tradeoffers.filter_by(|trade| trade.tradeid == tradeid))
            .await?
            .swap_remove(0);

        Ok(found_trade
            .every_asset()
            .into_iter()
            .map(|traded_asset| traded_asset.new_assetid)
            .collect::<Vec<_>>())
    }

    /// Convenience function to auto decline offers received.
    ///
    /// This will help keep the trade offers log clean of the total trade offer limit, if there is one.
    pub async fn decline_received_offers(&self) -> Result<(), TradeError> {
        let mut deny_offers_fut = FuturesOrdered::new();

        let active_received_offers: Vec<TradeOffer_Trade> = self
            .get_trade_offers(true, true, true)
            .map_ok(|tradeoffers| {
                tradeoffers.filter_by(|offer| offer.state == ETradeOfferState::Active && !offer.is_our_offer)
            })
            .await?;

        let total = active_received_offers.len();
        println!("{:#?}", active_received_offers);

        active_received_offers
            .into_iter()
            .map(|x| x.tradeofferid)
            .for_each(|x| {
                deny_offers_fut.push(
                    self.deny_offer(x)
                        .map_ok(|_| Delay::new(Duration::from_millis(STANDARD_DELAY))),
                );
            });

        while let Some(result) = deny_offers_fut.next().await {
            match result {
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }

        debug!("Successfully denied a total of {} received offers.", total);

        Ok(())
    }

    /// Creates a new trade offer, and confirms it with mobile authenticator.
    /// Returns the trade offer id on success and if the confirmation was not found but the trade created.
    ///
    /// It makes the assumption that the user has set up their ma file correctly.
    pub async fn create_offer_and_confirm(&self, tradeoffer: TradeOffer) -> Result<i64, TradeError> {
        let tradeoffer_id = self.create_offer(tradeoffer).await?;

        Delay::new(Duration::from_millis(STANDARD_DELAY)).await;

        let confirmations: Option<Confirmations> = self
            .authenticator
            .fetch_confirmations()
            .inspect_ok(|_| debug!("Confirmations fetched successfully."))
            .await?
            .map(|mut conf: Confirmations| {
                conf.filter_by_trade_offer_ids(&[tradeoffer_id]);
                conf
            });

        // If for some reason we end up not finding the confirmation, return an error
        if confirmations.is_none() {
            return Err(ConfirmationError::NotFoundButTradeCreated(tradeoffer_id).into());
        }

        self.authenticator
            .process_confirmations(ConfirmationMethod::Accept, confirmations.unwrap())
            .err_into()
            .await
            .map(|_| tradeoffer_id)
    }

    /// Convenience function to create a trade offer.
    /// Returns the trade offer id.
    pub async fn create_offer(&self, tradeoffer: TradeOffer) -> Result<i64, TradeError> {
        self.request::<TradeOfferCreateResponse>(TradeKind::Create(tradeoffer), None)
            .map_ok(|c| c.tradeofferid.map(|x| i64::from_str(&*x).unwrap()).unwrap())
            .await
    }

    /// Convenience function to accept a single trade offer that was made to this account.
    ///
    /// Note: It will confirm with the mobile authenticator, be extra careful when accepting any request.
    pub async fn accept_offer(&self, tradeoffer_id: i64) -> Result<(), TradeError> {
        let resp: TradeOfferCreateResponse = self.request(TradeKind::Accept, Some(tradeoffer_id)).await?;

        if resp.needs_mobile_confirmation.is_none() && !resp.needs_mobile_confirmation.unwrap() {
            return Ok(());
        }

        let confirmations: Option<Confirmations> = self
            .authenticator
            .fetch_confirmations()
            .inspect_ok(|_| debug!("Confirmations fetched successfully."))
            .await?
            .map(|mut conf: Confirmations| {
                debug!("{:#?}", conf);
                conf.filter_by_trade_offer_ids(&[tradeoffer_id]);
                conf
            });

        // If for some reason we end up not finding the confirmation, return an error
        if confirmations.is_none() {
            return Err(ConfirmationError::NotFound.into());
        }

        self.authenticator
            .process_confirmations(ConfirmationMethod::Accept, confirmations.unwrap())
            .err_into()
            .await
            .map(|_| ())
    }

    /// Convenience function to deny a single trade offer that was made to this account.
    ///
    /// # Errors
    ///
    /// Will error if couldn't deny the tradeoffer.
    pub async fn deny_offer(&self, tradeoffer_id: i64) -> Result<(), TradeError> {
        self.request::<TradeOfferCancelResponse>(TradeKind::Decline, Some(tradeoffer_id))
            .await
            .map(|_| ())
    }

    /// Convenience function to cancel a single trade offer that was created by this account.
    ///
    /// # Errors
    ///
    /// Will error if couldn't cancel the tradeoffer.
    pub async fn cancel_offer(&self, tradeoffer_id: i64) -> Result<(), TradeError> {
        self.request::<TradeOfferCancelResponse>(TradeKind::Cancel, Some(tradeoffer_id))
            .await
            .map(|_| ())
    }

    /// Check current session health, injects SessionID cookie, and send the request.
    async fn request<T>(&self, operation: TradeKind, tradeoffer_id: Option<i64>) -> Result<T, TradeError>
    where
        T: DeserializeOwned,
    {
        let tradeoffer_endpoint = operation.endpoint(tradeoffer_id);

        let mut header: Option<HeaderMap> = None;
        let mut partner_id_and_token = None;

        match &operation {
            TradeKind::Create(offer) => {
                header.replace(HeaderMap::new());
                header
                    .as_mut()
                    .unwrap()
                    .insert("Referer", (TRADEOFFER_BASE.to_owned() + "new").parse().unwrap());

                partner_id_and_token = Some((
                    offer.their_tradelink.partner_id.clone(),
                    offer.their_tradelink.token.clone(),
                ));
            }
            TradeKind::Accept => {
                header.replace(HeaderMap::new());
                header.as_mut().unwrap().insert(
                    "Referer",
                    format!("{}{}/", TRADEOFFER_BASE, tradeoffer_id.unwrap())
                        .parse()
                        .unwrap(),
                );
            }
            _ => {}
        };

        let mut request: Box<dyn HasSessionID> = match operation {
            TradeKind::Accept => {
                let partner_id = self
                    .get_tradeoffer_by_id(tradeoffer_id.unwrap())
                    .await?
                    .first()
                    .map(|c| SteamID::from_steam3(c.tradeofferid as u32, None, None))
                    .map(|steamid| steamid.to_steam64())
                    .ok_or(OfferError::NoMatch)?;

                let trade_request_data = TradeOfferAcceptRequest {
                    common: TradeOfferCommonParameters {
                        their_steamid: partner_id,
                        ..Default::default()
                    },
                    tradeofferid: tradeoffer_id.unwrap(),
                    ..Default::default()
                };

                debug!("{:#}", serde_json::to_string_pretty(&trade_request_data).unwrap());
                Box::new(trade_request_data)
            }

            TradeKind::Cancel | TradeKind::Decline => Box::new(TradeOfferGenericRequest::default()),
            TradeKind::Create(offer) => Box::new(Self::prepare_offer(offer)?),
        };

        // TODO: Check if session is ok, then inject cookie
        let session_id_cookie = self
            .authenticator
            .dump_cookie(STEAM_COMMUNITY_HOST, "sessionid")
            .ok_or_else(|| {
                PayloadError("Somehow you don't have a sessionid cookie. You need to login first.".to_string())
            })?;

        request.set_sessionid(session_id_cookie);

        let response_text: String = self
            .authenticator
            .request_custom_endpoint(tradeoffer_endpoint, Method::POST, header, Some(request))
            .and_then(|response| response.text())
            .inspect_ok(|resp_text: &String| debug!("{}", resp_text))
            .await?;

        match serde_json::from_str::<T>(&response_text) {
            Ok(response) => Ok(response),
            Err(_) => {
                // try to match into a generic message
                if let Ok(resp) = serde_json::from_str::<TradeOfferGenericErrorResponse>(&response_text) {
                    if resp.error_message.is_some() {
                        let err_msg = resp.error_message.unwrap();
                        Err(error_from_strmessage(&*err_msg).unwrap().into())
                    } else if resp.eresult.is_some() {
                        let eresult = resp.eresult.unwrap();
                        Err(tradeoffer_error_from_eresult(eresult).into())
                    } else {
                        tracing::error!("Unable to understand Steam Response. Please report it as bug.");
                        Err(OfferError::GeneralFailure(format!("Steam Response: {}", response_text)).into())
                    }
                } else {
                    if let Some((steamid, token)) = partner_id_and_token {
                        let steam_guard_result = check_steam_guard_error(self.authenticator, steamid, &*token).await;

                        if let Err(err) = steam_guard_result {
                            return Err(err);
                        }
                    }

                    tracing::error!(
                        "Failure to deserialize a valid response Steam Offer response. Maybe Steam Servers are \
                         offline."
                    );
                    Err(OfferError::GeneralFailure(format!("Steam Response: {}", response_text)).into())
                }
            }
        }
    }

    /// Checks that the tradeoffer is valid, and process it, getting the trade token and steamid3, into a
    /// `TradeOfferCreateRequest`, ready to send it.
    fn prepare_offer(tradeoffer: TradeOffer) -> Result<TradeOfferCreateRequest, TradeError> {
        TradeOffer::validate(&tradeoffer.my_assets, &tradeoffer.their_assets)?;

        let tradelink = tradeoffer.their_tradelink.clone();

        let their_steamid64 = tradelink.partner_id.to_steam64();
        let trade_offer_params = TradeOfferParams {
            trade_offer_access_token: tradelink.token,
        };

        Ok(TradeOfferCreateRequest::new(
            their_steamid64,
            tradeoffer,
            trade_offer_params,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_tradeoffer_url_with_token() -> &'static str {
        "https://steamcommunity.com/tradeoffer/new/?partner=79925588&token=Ob27qXzn"
    }

    fn get_tradeoffer_url_without_token() -> &'static str {
        "https://steamcommunity.com/tradeoffer/new/?partner=79925588"
    }

    fn sample_trade_history_response() -> GetTradeHistoryResponse {
        let response = r#"{
  "response": {
    "more": true,
    "trades": [
      {
        "tradeid": "3622543526924228084",
        "steamid_other": "76561198040191316",
        "time_init": 1603998438,
        "status": 3,
        "assets_given": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "15319724006",
            "amount": "1",
            "classid": "3035569977",
            "instanceid": "302028390",
            "new_assetid": "19793871926",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "3151905948742966439",
        "steamid_other": "76561198040191316",
        "time_init": 1594190957,
        "status": 3,
        "assets_received": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "17300115678",
            "amount": "1",
            "classid": "1989330488",
            "instanceid": "302028390",
            "new_assetid": "19034292089",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "3151905948742946486",
        "steamid_other": "76561198040191316",
        "time_init": 1594190486,
        "status": 3,
        "assets_received": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "17341684309",
            "amount": "1",
            "classid": "1989279043",
            "instanceid": "302028390",
            "new_assetid": "19034259977",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "3151905948734426645",
        "steamid_other": "76561198017653157",
        "time_init": 1593990409,
        "status": 3,
        "assets_received": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "8246208960",
            "amount": "1",
            "classid": "310776668",
            "instanceid": "302028390",
            "new_assetid": "19019879428",
            "new_contextid": "2"
          },
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "11589364986",
            "amount": "1",
            "classid": "469467368",
            "instanceid": "302028390",
            "new_assetid": "19019879441",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "2816382071757670028",
        "steamid_other": "76561198040191316",
        "time_init": 1587519425,
        "status": 3,
        "assets_received": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "17921552800",
            "amount": "1",
            "classid": "1989286992",
            "instanceid": "302028390",
            "new_assetid": "18426035472",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "2289455842905057389",
        "steamid_other": "76561198994791561",
        "time_init": 1582942255,
        "time_escrow_end": 1584238255,
        "status": 3,
        "assets_given": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "16832065568",
            "amount": "1",
            "classid": "1989312177",
            "instanceid": "302028390",
            "new_assetid": "18074934023",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "2022547174628342555",
        "steamid_other": "76561197966598809",
        "time_init": 1515645117,
        "status": 3,
        "assets_received": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "4345999",
            "amount": "1",
            "classid": "310777161",
            "instanceid": "188530139",
            "new_assetid": "13327873664",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "2022547174628335361",
        "steamid_other": "76561197976600825",
        "time_init": 1515644947,
        "status": 3,
        "assets_received": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "12792180950",
            "amount": "1",
            "classid": "2521767801",
            "instanceid": "0",
            "new_assetid": "13327860916",
            "new_contextid": "2"
          },
          {
            "appid": 447820,
            "contextid": "2",
            "assetid": "1667881814169014779",
            "amount": "1",
            "classid": "2219693199",
            "instanceid": "0",
            "new_assetid": "1827766562939536102",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "2022547174624411155",
        "steamid_other": "76561197971392179",
        "time_init": 1515552781,
        "status": 3,
        "assets_received": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "13213233361",
            "amount": "1",
            "classid": "2521767801",
            "instanceid": "0",
            "new_assetid": "13314519275",
            "new_contextid": "2"
          },
          {
            "appid": 447820,
            "contextid": "2",
            "assetid": "2364813217118906230",
            "amount": "1",
            "classid": "2219693201",
            "instanceid": "0",
            "new_assetid": "1827766492051349432",
            "new_contextid": "2"
          },
          {
            "appid": 578080,
            "contextid": "2",
            "assetid": "1807498550407081772",
            "amount": "1",
            "classid": "2451623575",
            "instanceid": "0",
            "new_assetid": "1827766492051349437",
            "new_contextid": "2"
          }
        ]
      },
      {
        "tradeid": "1640843092290105607",
        "steamid_other": "76561197998993178",
        "time_init": 1492806587,
        "status": 3,
        "assets_given": [
          {
            "appid": 730,
            "contextid": "2",
            "assetid": "4063307518",
            "amount": "1",
            "classid": "310779465",
            "instanceid": "188530139",
            "new_assetid": "9937692380",
            "new_contextid": "2"
          }
        ]
      }
    ]
  }
}
"#;
        serde_json::from_str::<GetTradeHistoryResponse>(&response).unwrap()
    }

    #[test]
    fn new_assets() {
        let raw_response = sample_trade_history_response();
        let filtered = raw_response.filter_by(|x| x.tradeid == 3622543526924228084).remove(0);
        let asset = filtered.every_asset().remove(0);
        assert_eq!(asset.assetid, 15319724006);
        assert_eq!(asset.new_assetid, 19793871926);
    }

    #[cfg(feature = "time")]
    #[test]
    fn estimate_time() {
        use crate::time::{estimate_tradelock_end, ONE_WEEK_SECONDS};

        let raw_response = sample_trade_history_response();
        let filtered_trade = raw_response.filter_by(|x| x.tradeid == 3622543526924228084).remove(0);
        let trade_completed_time = filtered_trade.time_init;
        assert_eq!(
            estimate_tradelock_end(trade_completed_time, ONE_WEEK_SECONDS).timestamp(),
            1604649600
        );
    }
}
