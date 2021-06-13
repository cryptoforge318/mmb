use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use futures::future::join_all;
use log::{error, info};
use tokio::sync::{broadcast, oneshot};

use crate::core::disposition_execution::executor::DispositionExecutorService;
use crate::core::exchanges::application_manager::ApplicationManager;
use crate::core::exchanges::cancellation_token::CancellationToken;
use crate::core::exchanges::common::{CurrencyPair, ExchangeAccountId, ExchangeId};
use crate::core::exchanges::events::{ExchangeEvent, ExchangeEvents, CHANNEL_MAX_EVENTS_COUNT};
use crate::core::exchanges::general::exchange_creation::create_exchange;
use crate::core::exchanges::general::{
    exchange::Exchange, exchange_creation::create_timeout_manager,
};
use crate::core::exchanges::timeouts::timeout_manager::TimeoutManager;
use crate::core::exchanges::traits::ExchangeClientBuilder;
use crate::core::internal_events_loop::InternalEventsLoop;
use crate::core::lifecycle::trading_engine::{EngineContext, TradingEngine};
use crate::core::logger::init_logger;
use crate::core::order_book::local_snapshot_service::LocalSnapshotsService;
use crate::core::settings::{AppSettings, BaseStrategySettings, CoreSettings};
use crate::hashmap;
use crate::strategies::disposition_strategy::DispositionStrategy;
use crate::{
    core::exchanges::binance::binance::BinanceBuilder, rest_api::control_panel::ControlPanel,
};
use std::convert::identity;

pub struct EngineBuildConfig {
    pub supported_exchange_clients: HashMap<ExchangeId, Box<dyn ExchangeClientBuilder + 'static>>,
}

impl EngineBuildConfig {
    pub fn standard() -> Self {
        let exchange_name = "Binance".into();
        let supported_exchange_clients =
            hashmap![exchange_name => Box::new(BinanceBuilder) as Box<dyn ExchangeClientBuilder>];

        EngineBuildConfig {
            supported_exchange_clients,
        }
    }
}

pub enum InitSettings<TStrategySettings>
where
    TStrategySettings: BaseStrategySettings + Clone,
{
    Directly(AppSettings<TStrategySettings>),
    Load,
}

pub async fn launch_trading_engine<TStrategySettings>(
    build_settings: &EngineBuildConfig,
    init_user_settings: InitSettings<TStrategySettings>,
) -> TradingEngine
where
    TStrategySettings: BaseStrategySettings + Clone,
{
    init_logger();

    info!("*****************************");
    info!("TradingEngine starting");

    let settings = match init_user_settings {
        InitSettings::Directly(v) => v,
        InitSettings::Load => load_settings::<TStrategySettings>().await,
    };

    let application_manager = ApplicationManager::new(CancellationToken::new());
    let (events_sender, events_receiver) = broadcast::channel(CHANNEL_MAX_EVENTS_COUNT);

    let timeout_manager = create_timeout_manager(&settings.core, &build_settings);
    let exchanges = create_exchanges(
        &settings.core,
        build_settings,
        events_sender.clone(),
        application_manager.clone(),
        &timeout_manager,
    )
    .await;

    let exchanges_map: DashMap<_, _> = exchanges
        .into_iter()
        .map(|exchange| (exchange.exchange_account_id.clone(), exchange))
        .collect();

    let exchange_events = ExchangeEvents::new(events_sender);

    let (finish_graceful_shutdown_tx, finish_graceful_shutdown_rx) = oneshot::channel();
    let engine_context = EngineContext::new(
        settings.core.clone(),
        exchanges_map.clone(),
        exchange_events,
        finish_graceful_shutdown_tx,
        timeout_manager,
        application_manager,
    );

    let internal_events_loop = InternalEventsLoop::new();
    let control_panel = ControlPanel::new("127.0.0.1:8080");

    {
        let local_exchanges_map = exchanges_map.into_iter().map(identity).collect();
        let _ = tokio::spawn(internal_events_loop.clone().start(
            events_receiver,
            local_exchanges_map,
            engine_context.application_manager.stop_token(),
        ));
    }

    if let Err(error) = control_panel.clone().start() {
        error!("Unable to start rest api: {}", error);
    }

    let strategy_settings = &settings.strategy as &dyn BaseStrategySettings;
    let disposition_executor_service = create_disposition_executor_service(
        strategy_settings.exchange_account_id(),
        strategy_settings.currency_pair(),
        &engine_context,
    );

    engine_context.shutdown_service.register_services(&[
        control_panel,
        internal_events_loop,
        disposition_executor_service,
    ]);

    info!("TradingEngine started");
    TradingEngine::new(engine_context, finish_graceful_shutdown_rx)
}

fn create_disposition_executor_service(
    exchange_account_id: ExchangeAccountId,
    currency_pair: CurrencyPair,
    engine_context: &Arc<EngineContext>,
) -> Arc<DispositionExecutorService> {
    DispositionExecutorService::new(
        engine_context.clone(),
        engine_context.get_events_channel(),
        LocalSnapshotsService::default(),
        exchange_account_id.clone(),
        currency_pair.clone(),
        Box::new(DispositionStrategy::new(exchange_account_id, currency_pair)),
        engine_context.application_manager.stop_token(),
    )
}

async fn load_settings<TSettings>() -> AppSettings<TSettings>
where
    TSettings: BaseStrategySettings + Clone,
{
    todo!("Not implemented")
}

pub async fn create_exchanges(
    core_settings: &CoreSettings,
    build_settings: &EngineBuildConfig,
    events_channel: broadcast::Sender<ExchangeEvent>,
    application_manager: Arc<ApplicationManager>,
    timeout_manager: &Arc<TimeoutManager>,
) -> Vec<Arc<Exchange>> {
    join_all(core_settings.exchanges.iter().map(|x| {
        create_exchange(
            x,
            build_settings,
            events_channel.clone(),
            application_manager.clone(),
            timeout_manager.clone(),
        )
    }))
    .await
}
