//! Order placement route. Steps 1-3 (symbol/filter validation) run before
//! any transaction opens; steps 4-9 (reservation, risk evaluation, order
//! insert) run inside one transaction.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use confluence_core::{evaluate, TradeIntent, TradeSide};
use confluence_exchange::types::{OrderIntent, OrderSide, OrderType, PriceEstimate, PriceSource};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::db;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::validators::validate_order;

#[derive(Deserialize)]
pub struct PlaceOrderRequest {
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
}

#[derive(Serialize)]
pub struct OrderView {
    pub id: Uuid,
    pub status: String,
}

pub async fn place_order(
    State(state): State<AppState>,
    Path(account_id): Path<Uuid>,
    Json(body): Json<PlaceOrderRequest>,
) -> Result<(StatusCode, Json<OrderView>), ApiError> {
    if body.symbol.trim().is_empty() {
        return Err(ApiError::Invalid("symbol must not be empty".into()));
    }
    if body.quantity <= Decimal::ZERO {
        return Err(ApiError::Invalid("quantity must be > 0".into()));
    }

    let intent = OrderIntent {
        symbol: body.symbol.clone(),
        side: body.side,
        order_type: body.order_type,
        quantity: body.quantity,
        price: body.price,
    };

    // Steps 1-3: fail fast, no DB round trip for invalid input.
    let symbol_meta = state
        .symbols
        .get(&body.symbol)
        .ok_or_else(|| ApiError::Invalid(format!("unknown or stale symbol metadata: {}", body.symbol)))?;

    let price_est: Option<PriceEstimate> = {
        let md = state.market_data.lock().unwrap();
        let is_buy = matches!(body.side, OrderSide::Buy);
        md.conservative_estimate(&body.symbol, is_buy, std::time::Instant::now())
            .map(|est| PriceEstimate {
                price: est.price,
                source: if is_buy { PriceSource::BestAsk } else { PriceSource::BestBid },
                is_fresh: est.is_fresh,
            })
    };
    validate_order(&intent, &symbol_meta, price_est)?;

    // Steps 4-9: one transaction.
    let mut tx = db::tenant_tx(&state.pool, account_id).await?;
    let account = db::lock_account(&mut tx, account_id).await?;
    let (risk_cfg, _) = db::load_risk_config(&mut tx, account_id).await?;
    let exchange_mode = db::load_exchange_mode(&mut tx, account_id).await?;
    let snapshot = db::load_snapshot(&mut tx, account_id, &account).await?;

    let trade_side = match body.side {
        OrderSide::Buy => TradeSide::Buy,
        OrderSide::Sell => TradeSide::Sell,
    };
    let evaluation_price = body.price.unwrap_or_else(|| price_est.map(|e| e.price).unwrap_or(Decimal::ZERO));
    let core_intent = TradeIntent {
        symbol: body.symbol.clone(),
        side: trade_side,
        quantity: body.quantity,
        price: evaluation_price,
    };

    let decision = evaluate(&core_intent, &snapshot, &risk_cfg);

    if !decision.is_approved() {
        db::insert_risk_event(
            &mut tx,
            account_id,
            "order_rejected",
            &json!({ "intent": core_intent, "decision": decision }),
        )
        .await?;
        tx.commit().await?;
        return Err(ApiError::Invalid(format!("order rejected: {decision:?}")));
    }

    let order_id = Uuid::now_v7();
    let client_order_id = order_id.to_string().replace('-', "");

    db::insert_order(
        &mut tx,
        order_id,
        account_id,
        &client_order_id,
        &body.symbol,
        body.side,
        body.order_type,
        body.quantity,
        body.price,
        exchange_mode,
    )
    .await?;

    db::insert_exchange_command(
        &mut tx,
        account_id,
        "submit_order",
        exchange_mode,
        &format!("{order_id}:submit"),
        &json!({ "order_id": order_id }),
    )
    .await?;

    db::insert_risk_event(
        &mut tx,
        account_id,
        "order_placed",
        &json!({ "order_id": order_id, "intent": core_intent }),
    )
    .await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(OrderView {
            id: order_id,
            status: "pending".into(),
        }),
    ))
}

