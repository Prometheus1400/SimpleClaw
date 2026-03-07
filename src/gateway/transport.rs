use std::collections::HashMap;
use std::sync::Arc;

use crate::channels::Channel;
use crate::config::GatewayChannelKind;
use crate::error::FrameworkError;

pub(super) fn channel_for_source<'a>(
    channels: &'a HashMap<GatewayChannelKind, Arc<dyn Channel>>,
    source: GatewayChannelKind,
) -> Result<&'a Arc<dyn Channel>, FrameworkError> {
    channels.get(&source).ok_or_else(|| {
        FrameworkError::Config(format!(
            "missing channel handler for source {}",
            source.as_str()
        ))
    })
}
