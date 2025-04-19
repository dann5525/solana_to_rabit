use std::{sync::mpsc::Receiver, time::Duration};
use anyhow::Result;
use lapin::{
    options::{BasicPublishOptions, ExchangeDeclareOptions, QueueDeclareOptions},
    types::FieldTable,
    BasicProperties, Connection, ConnectionProperties, ExchangeKind,
};
use quic_geyser_common::channel_message::ChannelMessage;

use tokio::time::sleep;

use crate::config::Config;


/// Example of a run_lavin_mq_loop with reconnection logic.
/// This version uses exchanges for account messages, using the account pubkey as routing key.
pub async fn run_lavin_mq_loop(amqp_url: &str, mq_rx: Receiver<ChannelMessage>, config: Config) -> Result<()> {

    

    let exchange_alternate = config.accounts_exchange_alternate.clone();
    let exchange_primary = config.accounts_exchange_primary.clone();
    let queue_accounts_unrouted = config.queue_accounts_unrouted.clone();
    let queue_transactions = config.queue_transactions.clone();
    let queue_blockmeta = config.queue_blockmeta.clone();
    
    

    'outer: loop {
        // 1) Connect to AMQP
        let conn = match Connection::connect(amqp_url, ConnectionProperties::default()).await {
            Ok(c) => c,
            Err(e) => {
                log::error!("Error connecting to AMQP: {e}, retrying in 5s...");
                sleep(Duration::from_secs(5)).await;
                continue 'outer;
            }
        };

        // 2) Create channel
        let channel = match conn.create_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                log::error!("Error creating channel: {e}, retrying in 5s...");
                sleep(Duration::from_secs(5)).await;
                continue 'outer;
            }
        };


        // 3) Declare exchanges for accounts (direct exchange with alternate exchange)
        let exchange_options: ExchangeDeclareOptions = ExchangeDeclareOptions {
            durable: true,
            ..Default::default()
        };

        // Define queue options here so it's available for both exchanges and queues
        let queue_options = QueueDeclareOptions {
            durable: true,
            ..Default::default()
        };

        // First declare the alternate exchange (must be created before the primary)
        if let Err(e) = channel
            .exchange_declare(
                &exchange_alternate, 
                ExchangeKind::Fanout,  // Fanout will capture all messages without needing specific routing keys
                exchange_options.clone(), 
                FieldTable::default()
            )
            .await
        {
            log::error!("Error declaring alternate exchange: {e}, retrying in 5s...");
            sleep(Duration::from_secs(5)).await;
            continue 'outer;
        }

        // Declare the primary exchange with alternate-exchange argument
        let mut args = FieldTable::default();
        // Use proper conversion for AMQPValue
        args.insert("alternate-exchange".into(), lapin::types::AMQPValue::LongString(exchange_alternate.clone().into()));

        if let Err(e) = channel
            .exchange_declare(
                &exchange_primary, 
                ExchangeKind::Direct,
                exchange_options.clone(), 
                args
            )
            .await
        {
            log::error!("Error declaring primary account exchange: {e}, retrying in 5s...");
            sleep(Duration::from_secs(5)).await;
            continue 'outer;
        }

        // Create a queue for unrouted messages
        if let Err(e) = channel
            .queue_declare(
                &queue_accounts_unrouted, 
                queue_options.clone(), 
                FieldTable::default()
            )
            .await
        {
            log::error!("Error declaring unrouted queue: {e}, retrying in 5s...");
            sleep(Duration::from_secs(5)).await;
            continue 'outer;
        }

        // Bind the unrouted queue to the alternate exchange
        if let Err(e) = channel
            .queue_bind(
                &queue_accounts_unrouted,
                &exchange_alternate,
                "", // Empty routing key for fanout exchange
                lapin::options::QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
        {
            log::error!("Error binding unrouted queue to alternate exchange: {e}, retrying in 5s...");
            sleep(Duration::from_secs(5)).await;
            continue 'outer;
        }

        // Declare regular queues for other message types
        let queue_options = QueueDeclareOptions {
            durable: true,
            ..Default::default()
        };

        for queue_name in [&queue_transactions, &queue_blockmeta] {
            if let Err(e) = channel
                .queue_declare(queue_name, queue_options.clone(), FieldTable::default())
                .await
            {
                log::error!("Error declaring queue {}: {}", queue_name, e);
                sleep(Duration::from_secs(5)).await;
                continue 'outer;
            }
        }

        log::info!("Connected to AMQP and declared exchanges/queues successfully.");

        // 4) Process messages
        while let Ok(msg) = mq_rx.recv() {
            match msg {
                ChannelMessage::Transaction(tx) => {
                    let payload = match serde_json::to_vec(&tx) {
                        Ok(p) => p,
                        Err(serde_err) => {
                            log::error!("Failed to serialize transaction: {serde_err}");
                            continue;
                        }
                    };

                    if let Err(e) = publish_message_to_queue(&channel, &queue_transactions, &payload).await {
                        log::error!("AMQP publish error for transaction: {e}");
                        sleep(Duration::from_secs(5)).await;
                        continue 'outer;
                    }
                }
                ChannelMessage::Account(account_data, slot, is_startup) => {
                    // Create a structure to serialize account data with metadata
                    let account_message = serde_json::json!({
                        "account": {
                            "pubkey": account_data.pubkey.to_string(),
                            "lamports": account_data.account.lamports,
                            "owner": account_data.account.owner.to_string(),
                            "executable": account_data.account.executable,
                            "rentEpoch": account_data.account.rent_epoch,
                            "data": account_data.account.data,
                        },
                        "slot": slot,
                        "isStartup": is_startup,
                        "writeVersion": account_data.write_version,
                    });

                    let payload = match serde_json::to_vec(&account_message) {
                        Ok(p) => p,
                        Err(serde_err) => {
                            log::error!("Failed to serialize account data: {serde_err}");
                            continue;
                        }
                    };

                   
                    
                    // Also add program (owner) as a routing tag
                    let program_routing_key = account_data.account.owner.to_string();
                    let public_key = account_data.pubkey.to_string();
                   
                    // Add detailed logging before publishing
                    log::info!("Publishing to exchange: {}, routing key: {}, pubkey: {}", 
                    exchange_primary.clone(), &program_routing_key, &public_key);


                    // Also publish with program-based routing key
                    if let Err(e) = publish_message_to_exchange(&channel, &exchange_primary, &program_routing_key, &payload).await {
                        log::error!("AMQP publish error for account change (program routing): {e}");
                        sleep(Duration::from_secs(5)).await;
                        continue 'outer;
                    }
                }

                ChannelMessage::BlockMeta(block_meta) => {
                    let payload = match serde_json::to_vec(&block_meta) {
                        Ok(p) => p,
                        Err(serde_err) => {
                            log::error!("Failed to serialize block metadata: {serde_err}");
                            continue;
                        }
                    };

                    if let Err(e) = publish_message_to_queue(&channel, &queue_blockmeta, &payload).await {
                        log::error!("AMQP publish error for block metadata: {e}");
                        sleep(Duration::from_secs(5)).await;
                        continue 'outer;
                    }
                }
                // Handle other message types if needed
                other => {
                    log::debug!("Received other ChannelMessage type: {:?}", other);
                }
            }
        }

        log::warn!("mq_rx closed, shutting down lavin MQ loop");
        break 'outer;
    }

    Ok(())
}

// Function for publishing to queues (for transactions and block metadata)
async fn publish_message_to_queue(channel: &lapin::Channel, queue: &str, payload: &[u8]) -> Result<()> {
    let confirm = channel
        .basic_publish(
            "",
            queue,
            BasicPublishOptions::default(),
            payload,
            BasicProperties::default(),
        )
        .await?
        .await?;

    if confirm.is_nack() {
        return Err(anyhow::anyhow!("Broker did not acknowledge message"));
    }

    Ok(())
}

// New function for publishing to exchanges with routing keys
async fn publish_message_to_exchange(
    channel: &lapin::Channel, 
    exchange: &str, 
    routing_key: &str, 
    payload: &[u8]
) -> Result<()> {
    let confirm = channel
        .basic_publish(
            exchange,
            routing_key,
            BasicPublishOptions::default(),
            payload,
            BasicProperties::default(),
        )
        .await?
        .await?;

    if confirm.is_nack() {
        return Err(anyhow::anyhow!("Broker did not acknowledge message"));
    }

    Ok(())
}

