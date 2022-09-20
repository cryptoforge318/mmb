use anyhow::Result;
use arrayvec::ArrayVec;
use dashmap::DashMap;
use function_name::named;
use hmac::{Hmac, Mac};
use hyper::http::request::Builder;
use hyper::Uri;
use mmb_core::exchanges::general::features::{
    BalancePositionOption, ExchangeFeatures, OpenOrdersType, OrderFeatures, OrderTradeOption,
    RestFillsFeatures, RestFillsType, WebSocketOptions,
};
use mmb_core::exchanges::hosts::Hosts;
use mmb_core::exchanges::rest_client::{
    ErrorHandlerData, ErrorHandlerEmpty, RequestType, RestClient, RestHeaders, RestResponse,
    UriBuilder,
};
use mmb_core::exchanges::timeouts::requests_timeout_manager_factory::RequestTimeoutArguments;
use mmb_core::exchanges::timeouts::timeout_manager::TimeoutManager;
use mmb_core::exchanges::traits::{
    ExchangeClientBuilder, ExchangeClientBuilderResult, ExchangeError, HandleOrderFilledCb,
    HandleTradeCb, OrderCancelledCb, OrderCreatedCb, Support,
};
use mmb_core::lifecycle::app_lifetime_manager::AppLifetimeManager;
use mmb_core::settings::ExchangeSettings;
use mmb_domain::events::{AllowedEventSourceType, ExchangeEvent};
use mmb_domain::exchanges::symbol::{Precision, Symbol};
use mmb_domain::market::{
    CurrencyCode, CurrencyId, CurrencyPair, ExchangeId, SpecificCurrencyPair,
};
use mmb_domain::order::pool::{OrderRef, OrdersPool};
use mmb_domain::order::snapshot::{
    Amount, ExchangeOrderId, OrderExecutionType, OrderSide, OrderType, Price,
};
use parking_lot::{Mutex, RwLock};
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::Sha256;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tinyvec::Array;
use tokio::sync::broadcast::Sender;

struct RestHeadersBitmex {
    api_key: String,
    secret_key: String,
}

impl RestHeadersBitmex {
    pub fn new(api_key: String, secret_key: String) -> Self {
        Self {
            api_key,
            secret_key,
        }
    }

    pub(super) fn get_signature(
        &self,
        path: &str,
        request_type: RequestType,
        expire_time: u64,
    ) -> [u8; 64] {
        let mut hmac = Hmac::<Sha256>::new_from_slice(self.secret_key.as_bytes())
            .expect("Unable to calculate hmac for Bitmex signature");
        hmac.update(request_type.as_str().as_bytes());
        hmac.update(path.as_bytes());

        let mut expire_time_array = ArrayVec::<u8, 20>::new();
        write!(expire_time_array, "{expire_time}").expect("Failed to convert UNIX time to string");
        hmac.update(expire_time_array.as_slice());

        let hmac_bytes = hmac.finalize().into_bytes();

        let mut hex_array = [0u8; 64];
        write!(hex_array.as_slice_mut(), "{:x}", hmac_bytes)
            .expect("Failed to convert signature bytes array to hex");

        hex_array
    }

    fn get_key_expire_time() -> u64 {
        const SECS_TO_EXPIRE: u64 = 60;

        let current_unix_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System Time before UNIX EPOCH!")
            .as_secs();

        current_unix_time + SECS_TO_EXPIRE
    }
}

impl RestHeaders for RestHeadersBitmex {
    fn add_specific_headers(
        &self,
        builder: Builder,
        uri: &Uri,
        request_type: RequestType,
    ) -> Builder {
        let expire_time = RestHeadersBitmex::get_key_expire_time();
        builder
            .header("api-expires", expire_time)
            .header("api-key", &self.api_key)
            .header(
                "api-signature",
                self.get_signature(uri.path(), request_type, expire_time)
                    .as_slice(),
            )
    }
}

const EMPTY_RESPONSE_IS_OK: bool = false;

// TODO Remove #[allow(dead_code)] after Bitmex exchange client will be implemented
#[allow(dead_code)]
pub struct Bitmex {
    pub(crate) settings: ExchangeSettings,
    pub hosts: Hosts,
    // TODO Replace ErrorHandlerEmpty with specific for Bitmex
    rest_client: RestClient<ErrorHandlerEmpty, RestHeadersBitmex>,
    pub(crate) unified_to_specific: RwLock<HashMap<CurrencyPair, SpecificCurrencyPair>>,
    specific_to_unified: RwLock<HashMap<SpecificCurrencyPair, CurrencyPair>>,
    pub(crate) supported_currencies: DashMap<CurrencyId, CurrencyCode>,
    // Currencies used for trading according to user settings
    pub(super) traded_specific_currencies: Mutex<Vec<SpecificCurrencyPair>>,
    lifetime_manager: Arc<AppLifetimeManager>,
    events_channel: Sender<ExchangeEvent>,
    pub(crate) order_created_callback: OrderCreatedCb,
    pub(crate) order_cancelled_callback: OrderCancelledCb,
    pub(crate) handle_order_filled_callback: HandleOrderFilledCb,
    pub(crate) handle_trade_callback: HandleTradeCb,
}

impl Bitmex {
    pub fn new(
        settings: ExchangeSettings,
        events_channel: Sender<ExchangeEvent>,
        lifetime_manager: Arc<AppLifetimeManager>,
    ) -> Bitmex {
        Self {
            rest_client: RestClient::new(
                ErrorHandlerData::new(
                    EMPTY_RESPONSE_IS_OK,
                    settings.exchange_account_id,
                    ErrorHandlerEmpty::default(),
                ),
                RestHeadersBitmex::new(settings.api_key.clone(), settings.secret_key.clone()),
            ),
            settings,
            hosts: Self::make_hosts(),
            unified_to_specific: Default::default(),
            specific_to_unified: Default::default(),
            supported_currencies: Default::default(),
            traded_specific_currencies: Default::default(),
            events_channel,
            lifetime_manager,
            order_created_callback: Box::new(|_, _, _| {}),
            order_cancelled_callback: Box::new(|_, _, _| {}),
            handle_order_filled_callback: Box::new(|_| {}),
            handle_trade_callback: Box::new(|_, _, _, _, _, _| {}),
        }
    }

    fn make_hosts() -> Hosts {
        Hosts {
            web_socket_host: "wss://www.bitmex.com/realtime",
            web_socket2_host: "wss://www.bitmex.com/realtime",
            rest_host: "https://www.bitmex.com",
        }
    }

    #[named]
    pub(super) async fn request_all_symbols(&self) -> Result<RestResponse, ExchangeError> {
        let builder = UriBuilder::from_path("/api/v1/instrument/activeAndIndices");
        let uri = builder.build_uri(self.hosts.rest_uri_host(), false);

        self.rest_client
            .get(uri, function_name!(), "".to_string())
            .await
    }

    pub(super) fn parse_all_symbols(&self, response: &RestResponse) -> Result<Vec<Arc<Symbol>>> {
        let symbols: Vec<BitmexSymbol> = serde_json::from_str(&response.content)
            .expect("Unable to deserialize response from Bitmex");
        let mut supported_symbols = Vec::new();

        for symbol in &symbols {
            if Bitmex::is_unsupported_symbol(symbol) {
                continue;
            }

            let base = symbol.base_id.as_str().into();
            let quote = symbol.quote_id.as_str().into();

            let specific_currency_pair = symbol.id.as_str().into();
            let unified_currency_pair = CurrencyPair::from_codes(base, quote);
            self.unified_to_specific
                .write()
                .insert(unified_currency_pair, specific_currency_pair);

            let (amount_currency_code, balance_currency_code) = if symbol.id != "XBTUSD" {
                (base, None)
            } else {
                (CurrencyCode::from("XBT"), Some(CurrencyCode::from("BTC")))
            };

            let price_tick = symbol.price_tick.expect("Null price tick value");
            let amount_tick = symbol.amount_tick.expect("Null amount tick value");

            let symbol = Symbol::new(
                self.settings.is_margin_trading,
                symbol.base_id.as_str().into(),
                base,
                symbol.quote_id.as_str().into(),
                quote,
                None,
                symbol.max_price,
                None,
                symbol.max_amount,
                None,
                amount_currency_code,
                balance_currency_code,
                Precision::ByTick { tick: price_tick },
                Precision::ByTick { tick: amount_tick },
            );

            supported_symbols.push(Arc::new(symbol));
        }

        Ok(supported_symbols)
    }

    fn is_unsupported_symbol(symbol: &BitmexSymbol) -> bool {
        let is_inactive_symbol = symbol.state == "Unlisted";

        // Symbols list has the same CurrencyCodePair for all the BTC/USD futures, we keep only perpetual swap for now
        let is_unsupported_futures =
            symbol.base_id == "BTC" && symbol.quote_id == "USD" && symbol.id != "XBTUSD";

        is_inactive_symbol || is_unsupported_futures
    }

    #[named]
    pub(super) async fn request_create_order(
        &self,
        order: &OrderRef,
    ) -> Result<RestResponse, ExchangeError> {
        let (header, price, stop_loss_price, mut trailing_stop_delta) = order.fn_ref(|order| {
            (
                order.header.clone(),
                order.price(),
                order.props.stop_loss_price,
                order.props.trailing_stop_delta,
            )
        });
        let specific_currency_pair = self.get_specific_currency_pair(header.currency_pair);

        let mut builder = UriBuilder::from_path("/api/v1/order");
        builder.add_kv("symbol", specific_currency_pair);
        builder.add_kv("side", header.side.as_str());
        builder.add_kv("orderQty", header.amount);
        builder.add_kv("clOrdID", header.client_order_id.as_str());

        match header.order_type {
            OrderType::Market => builder.add_kv("ordType", "Market"),
            OrderType::Limit => {
                builder.add_kv("ordType", "Limit");
                builder.add_kv("price", price);
                if header.execution_type == OrderExecutionType::MakerOnly {
                    builder.add_kv("execInst", "ParticipateDoNotInitiate");
                }
            }
            OrderType::StopLoss => {
                builder.add_kv("ordType", "Stop");
                builder.add_kv("stopPx", stop_loss_price);
            }
            OrderType::TrailingStop => {
                builder.add_kv("ordType", "Stop");
                builder.add_kv("pegPriceType", "TrailingStopPeg");
                if header.side == OrderSide::Sell {
                    trailing_stop_delta.set_sign_negative(true);
                }
                builder.add_kv("pegOffsetValue", trailing_stop_delta);
            }
            OrderType::ClosePosition => {
                // It will cancel other active limit orders with the same side and symbol if the open quantity exceeds the current position
                // Details: https://www.bitmex.com/api/explorer/#!/Order/Order_new
                builder.add_kv("ordType", "Close");
            }
            _ => return Err(ExchangeError::unknown("Unexpected order type")),
        }

        let (uri, query) = builder.build_uri_and_query(self.hosts.rest_uri_host(), false);
        let log_args = format!("Create order for {header:?}");
        self.rest_client
            .post(uri, query, function_name!(), log_args)
            .await
    }

    pub(super) fn get_order_id(
        &self,
        response: &RestResponse,
    ) -> Result<ExchangeOrderId, ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename = "orderID")]
        struct OrderId<'a> {
            order_id: &'a str,
        }

        let deserialized: OrderId = serde_json::from_str(&response.content)
            .map_err(|err| ExchangeError::parsing(format!("Unable to parse orderId: {err:?}")))?;

        Ok(ExchangeOrderId::from(deserialized.order_id))
    }
}

#[derive(Deserialize, Debug)]
struct BitmexSymbol {
    #[serde(rename = "symbol")]
    id: String,
    #[serde(rename = "underlying")]
    base_id: String,
    #[serde(rename = "quoteCurrency")]
    quote_id: String,
    state: String,
    #[serde(rename = "tickSize")]
    price_tick: Option<Decimal>,
    #[serde(rename = "lotSize")]
    amount_tick: Option<Decimal>,
    #[serde(rename = "maxPrice")]
    max_price: Option<Price>,
    #[serde(rename = "maxOrderQty")]
    max_amount: Option<Amount>,
}

pub struct BitmexBuilder;

impl ExchangeClientBuilder for BitmexBuilder {
    fn create_exchange_client(
        &self,
        exchange_settings: ExchangeSettings,
        events_channel: Sender<ExchangeEvent>,
        lifetime_manager: Arc<AppLifetimeManager>,
        _timeout_manager: Arc<TimeoutManager>,
        _orders: Arc<OrdersPool>,
    ) -> ExchangeClientBuilderResult {
        ExchangeClientBuilderResult {
            client: Box::new(Bitmex::new(
                exchange_settings,
                events_channel,
                lifetime_manager,
            )),
            features: ExchangeFeatures {
                open_orders_type: OpenOrdersType::AllCurrencyPair,
                rest_fills_features: RestFillsFeatures::new(RestFillsType::MyTrades),
                order_features: OrderFeatures {
                    maker_only: true,
                    supports_get_order_info_by_client_order_id: true,
                    cancellation_response_from_rest_only_for_errors: true,
                    creation_response_from_rest_only_for_errors: true,
                    order_was_completed_error_for_cancellation: true,
                    supports_already_cancelled_order: true,
                    supports_stop_loss_order: true,
                },
                trade_option: OrderTradeOption {
                    supports_trade_time: true,
                    supports_trade_incremented_id: false,
                    notification_on_each_currency_pair: false,
                    supports_get_prints: true,
                    supports_tick_direction: true,
                    supports_my_trades_from_time: true,
                },
                websocket_options: WebSocketOptions {
                    execution_notification: true,
                    cancellation_notification: true,
                    supports_ping_pong: true,
                    supports_subscription_response: false,
                },
                empty_response_is_ok: EMPTY_RESPONSE_IS_OK,
                balance_position_option: BalancePositionOption::NonDerivative,
                allowed_create_event_source_type: AllowedEventSourceType::All,
                allowed_fill_event_source_type: AllowedEventSourceType::All,
                allowed_cancel_event_source_type: AllowedEventSourceType::All,
            },
        }
    }

    fn get_timeout_arguments(&self) -> RequestTimeoutArguments {
        RequestTimeoutArguments::from_requests_per_minute(60)
    }

    fn get_exchange_id(&self) -> ExchangeId {
        "Bitmex".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstr::ByteSlice;

    #[test]
    fn generate_signature() {
        // Test data from https://www.bitmex.com/app/apiKeysUsage
        let api_key = "LAqUlngMIQkIUjXMUreyu3qn".to_owned();
        let secret_key = "chNOOS4KvNXR_Xq4k4c9qsfoKWvnDecLATCRlcBwyKDYnWgO".to_owned();
        let path = "/api/v1/instrument";
        let expire_time = 1518064236;

        let rest_header = RestHeadersBitmex {
            api_key,
            secret_key,
        };

        let signature_hash = rest_header.get_signature(path, RequestType::Get, expire_time);

        assert_eq!(
            signature_hash
                .to_str()
                .expect("Failed to convert signature hash to string"),
            "c7682d435d0cfe87c16098df34ef2eb5a549d4c5a3c2b1f0f77b8af73423bf00"
        );
    }
}