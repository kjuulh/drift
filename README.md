# nodrift

A lightweight Rust library for scheduling recurring jobs with automatic drift correction. When a job takes longer than expected, nodrift ensures the next execution adjusts accordingly to maintain consistent intervals.

## Features

- **Drift correction**: Automatically adjusts scheduling when jobs run longer than the interval
- **Async/await support**: Built on Tokio for efficient async job execution
- **Cancellation support**: Gracefully stop scheduled jobs using cancellation tokens
- **Custom job types**: Implement the `Drifter` trait for complex scheduling needs
- **Error handling**: Jobs can fail without crashing the scheduler
- **Detailed logging**: Built-in tracing support for debugging

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
nodrift = "0.3.1"
tokio = { version = "1", features = ["full"] }
```

## Quick Start

### Simple Function Scheduling

The easiest way to schedule a recurring job is using the `schedule` function:

```rust
use nodrift::{schedule, DriftError};
use std::time::Duration;

#[tokio::main]
async fn main() {
    // Schedule a job to run every 5 seconds
    let cancellation_token = schedule(Duration::from_secs(5), || async {
        println!("Job executing!");
        
        // Your job logic here
        do_work().await?;
        
        Ok(())
    });

    // Let it run for a while
    tokio::time::sleep(Duration::from_secs(30)).await;
    
    // Cancel the job when done
    cancellation_token.cancel();
}

async fn do_work() -> Result<(), DriftError> {
    // Simulate some work
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}
```

### Custom Drifter Implementation

For more complex scheduling needs, implement the `Drifter` trait:

```rust
use nodrift::{Drifter, schedule_drifter, DriftError};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use std::time::Duration;

#[derive(Clone)]
struct MyJob {
    name: String,
}

#[async_trait]
impl Drifter for MyJob {
    async fn execute(&self, token: CancellationToken) -> anyhow::Result<()> {
        println!("Executing job: {}", self.name);
        
        // Check for cancellation during long-running operations
        if token.is_cancelled() {
            return Ok(());
        }
        
        // Your job logic here
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let job = MyJob {
        name: "data-sync".to_string(),
    };
    
    // Schedule the custom job
    let token = schedule_drifter(Duration::from_secs(10), job);
    
    // Run for a minute
    tokio::time::sleep(Duration::from_secs(60)).await;
    
    // Stop the job
    token.cancel();
}
```

## How Drift Correction Works

nodrift automatically handles timing drift that occurs when jobs take longer than expected:

1. If a job is scheduled to run every 5 seconds but takes 3 seconds to complete, the next run will start 2 seconds after completion
2. If a job takes 7 seconds to complete (longer than the 5-second interval), the next run starts immediately after completion
3. This ensures your jobs maintain their intended frequency without overlapping executions

## Error Handling

Jobs that return errors will stop the scheduling routine:

```rust
use nodrift::{schedule, DriftError};
use std::time::Duration;

let token = schedule(Duration::from_secs(1), || async {
    // This will stop the scheduler after the first execution
    Err(DriftError::JobError(anyhow::anyhow!("Something went wrong")))
});
```

## Logging

nodrift uses the `tracing` crate for logging. Enable logging to see job execution details:

```rust
use tracing_subscriber;

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();
    
    // Your application code
}
```

Log output includes:
- Job start times
- Execution duration
- Wait time until next run
- Next scheduled run time
- Any errors that occur

## Examples

### Database Cleanup Job

```rust
use nodrift::{schedule, DriftError};
use std::time::Duration;

async fn cleanup_old_records() -> Result<(), DriftError> {
    // Connect to database and clean up old records
    println!("Cleaning up old records...");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::main]
async fn main() {
    // Run cleanup every hour
    let token = schedule(Duration::from_secs(3600), || async {
        cleanup_old_records().await
    });
    
    // Keep the application running
    tokio::signal::ctrl_c().await.unwrap();
    
    // Gracefully shutdown
    token.cancel();
}
```

### Metrics Collection

```rust
use nodrift::{Drifter, schedule_drifter};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone)]
struct MetricsCollector {
    counter: Arc<AtomicU64>,
}

#[async_trait]
impl Drifter for MetricsCollector {
    async fn execute(&self, _token: CancellationToken) -> anyhow::Result<()> {
        // Collect metrics
        let value = self.counter.fetch_add(1, Ordering::Relaxed);
        println!("Collected metric: {}", value);
        
        // Send to monitoring service
        // send_to_monitoring(value).await?;
        
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let collector = MetricsCollector {
        counter: Arc::new(AtomicU64::new(0)),
    };
    
    // Collect metrics every 30 seconds
    let token = schedule_drifter(
        Duration::from_secs(30),
        collector
    );
    
    // Run indefinitely
    tokio::signal::ctrl_c().await.unwrap();
    token.cancel();
}
```

## API Reference

### Functions

#### `schedule<F, Fut>(interval: Duration, func: F) -> CancellationToken`

Schedules a function to run at the specified interval.

- `interval`: Time between job executions
- `func`: Async function that returns `Result<(), DriftError>`
- Returns: `CancellationToken` to stop the scheduled job

#### `schedule_drifter<FDrifter>(interval: Duration, drifter: FDrifter) -> CancellationToken`

Schedules a custom `Drifter` implementation.

- `interval`: Time between job executions
- `drifter`: Implementation of the `Drifter` trait
- Returns: `CancellationToken` to stop the scheduled job

### Traits

#### `Drifter`

```rust
#[async_trait]
pub trait Drifter {
    async fn execute(&self, token: CancellationToken) -> anyhow::Result<()>;
}
```

Implement this trait for custom job types that need access to state or complex initialization.

### Types

#### `DriftError`

Error type for job execution failures:

```rust
pub enum DriftError {
    JobError(#[source] anyhow::Error),
}
```

## Requirements

- Rust 1.75 or later
- Tokio runtime

## License

This project is licensed under the MIT License.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Support

For issues and questions, please file an issue on the [GitHub repository](https://github.com/kjuulh/drift).
