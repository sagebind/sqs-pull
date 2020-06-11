use indicatif::ProgressBar;
use regex::Regex;
use rusoto_core::Region;
use rusoto_sqs::{
    DeleteMessageBatchRequest,
    DeleteMessageBatchRequestEntry,
    GetQueueAttributesRequest,
    GetQueueUrlRequest,
    ReceiveMessageRequest,
    SendMessageBatchRequest,
    SendMessageBatchRequestEntry,
    Sqs,
    SqsClient,
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    sync::Arc,
};
use structopt::StructOpt;
use tokio::{sync::Mutex, task};

#[derive(Clone, Debug, StructOpt)]
struct Options {
    /// Name of the SQS queue to move messages from.
    source: String,

    /// Name of the destination SQS queue to move messages to.
    destination: String,

    /// The AWS region to use.
    #[structopt(short, long)]
    region: Option<Region>,

    /// Only move messages whose body matches the given regular expression.
    #[structopt(long)]
    body_matching: Option<Regex>,

    /// Exclude messages whose body matches the given regular expression.
    #[structopt(long)]
    body_not_matching: Option<Regex>,

    /// Number of messages to move in a single operation
    #[structopt(long, default_value = "10")]
    batch_size: u32,

    /// Number of workers to spawn to perform moving.
    ///
    /// Defaults to 4 per CPU core.
    #[structopt(short = "j", long)]
    worker_count: Option<usize>,

    /// Silence all command output.
    #[structopt(short, long)]
    quiet: bool,

    /// Verbose mode (-v, -vv, -vvv, etc)
    #[structopt(short = "v", long, parse(from_occurrences))]
    verbose: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::from_args();

    stderrlog::new()
        .quiet(options.quiet)
        .verbosity(options.verbose)
        .init()
        .unwrap();

    let task_count = options.worker_count
        .unwrap_or_else(|| num_cpus::get() * 4);

    let region = options.region.clone().unwrap_or_default();
    let sqs = SqsClient::new(region);

    let source_url = sqs.get_queue_url(GetQueueUrlRequest {
        queue_name: options.source.clone(),
        queue_owner_aws_account_id: None,
    }).await?.queue_url.unwrap();

    let destination_url = sqs.get_queue_url(GetQueueUrlRequest {
        queue_name: options.destination.clone(),
        queue_owner_aws_account_id: None,
    }).await?.queue_url.unwrap();

    println!("Source URL: {}", source_url);
    println!("Destination URL: {}", destination_url);
    println!("Using {} workers", task_count);

    // Try to guess how many messages we will be moving in order to show a progress bar.
    let source_attributes = sqs.get_queue_attributes(GetQueueAttributesRequest {
        queue_url: source_url.clone(),
        attribute_names: Some(vec![String::from("ApproximateNumberOfMessages")]),
    }).await?.attributes;

    let message_count_estimate = source_attributes
        .and_then(|map| map.get("ApproximateNumberOfMessages").cloned())
        .and_then(|s| s.parse::<u32>().ok());

    let progress_bar = match message_count_estimate {
        Some(count) => ProgressBar::new(count.into()),
        None => unimplemented!(),
    };

    // Keep track of message IDs we've seen so that we don't handle duplicate
    // messages and never stop.
    let seen_messages = Arc::new(Mutex::new(HashSet::new()));

    let mut worker_handles = Vec::new();

    for _ in 0..task_count {
        let task = move_messages(
            sqs.clone(),
            options.clone(),
            progress_bar.clone(),
            source_url.clone(),
            destination_url.clone(),
            seen_messages.clone()
        );

        worker_handles.push(task::spawn(async move {
            task.await.map_err(|e| e.to_string())
        }));
    }

    for handle in worker_handles {
        handle.await??;
    }

    progress_bar.finish_at_current_pos();

    Ok(())
}

async fn move_messages(
    sqs: SqsClient,
    options: Options,
    progress_bar: ProgressBar,
    source_url: String,
    destination_url: String,
    seen_messages: Arc<Mutex<HashSet<String>>>,
) -> Result<(), Box<dyn Error>> {
    let mut message_receipt_handles = HashMap::new();

    loop {
        message_receipt_handles.clear();

        let receive_result = sqs.receive_message(ReceiveMessageRequest {
            queue_url: source_url.to_owned(),
            max_number_of_messages: Some(options.batch_size.into()),
            attribute_names: None,
            message_attribute_names: None,
            receive_request_attempt_id: None,
            visibility_timeout: None,
            wait_time_seconds: Some(0),
        }).await?;

        let messages = if let Some(messages) = receive_result.messages {
            messages
        } else {
            return Ok(());
        };

        if messages.is_empty() {
            return Ok(());
        }

        let mut messages_to_move = Vec::new();

        for message in messages {
            // Save the message receipt handle for later.
            message_receipt_handles.insert(
                message.message_id.clone().unwrap(),
                message.receipt_handle.clone().unwrap(),
            );

            // Filter out messages we've seen before.
            if !seen_messages.lock().await.insert(message.message_id.clone().unwrap()) {
                continue;
            }

            // Apply body matching filter.
            if let Some(regex) = options.body_matching.as_ref() {
                if !message.body.as_ref()
                    .map(|body| regex.is_match(body))
                    .unwrap_or(false) {
                        continue;
                    }
            }

            // Apply body not matching filter.
            if let Some(regex) = options.body_not_matching.as_ref() {
                if !message.body.as_ref()
                    .map(|body| !regex.is_match(body))
                    .unwrap_or(true) {
                        continue;
                    }
            }

            messages_to_move.push(SendMessageBatchRequestEntry {
                // Use the message ID as the batch ID.
                id: message.message_id.unwrap(),

                message_body: message.body.unwrap(),
                ..Default::default()
            });
        }

        if messages_to_move.is_empty() {
            continue;
        }

        let send_result = sqs.send_message_batch(SendMessageBatchRequest {
            queue_url: destination_url.to_owned(),
            entries: messages_to_move,
        }).await?;

        if !send_result.failed.is_empty() {
            log::warn!("failed to send {} messages, leaving them in src queue", send_result.failed.len());
        }

        if send_result.successful.is_empty() {
            continue;
        }

        // Delete all the messages from the source queue which were sent successfully.
        let delete_result = sqs.delete_message_batch(DeleteMessageBatchRequest {
            queue_url: source_url.to_owned(),
            entries: send_result.successful.into_iter()
                .map(|send_entry| DeleteMessageBatchRequestEntry {
                    // Lookup the receipt handle from earlier.
                    receipt_handle: message_receipt_handles.get(&send_entry.id).unwrap().clone(),

                    // Use the original message ID again as the delete ID.
                    id: send_entry.id,
                })
                .collect(),
        }).await?;

        if !delete_result.failed.is_empty() {
            log::warn!("failed to delete {} messages, leaving them in src queue", delete_result.failed.len());
        }

        progress_bar.inc(delete_result.successful.len() as u64);
    }
}
