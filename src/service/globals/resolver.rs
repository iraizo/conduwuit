use std::{
	collections::HashMap,
	future, iter,
	net::{IpAddr, SocketAddr},
	sync::{Arc, RwLock as StdRwLock},
};

use hickory_resolver::TokioAsyncResolver;
use hyper::client::connect::dns::Name;
use reqwest::dns::{Addrs, Resolve, Resolving};
use ruma::OwnedServerName;
use tokio::sync::RwLock;
use tracing::error;

use crate::{api::server_server::FedDest, Config, Error};

pub type WellKnownMap = HashMap<OwnedServerName, (FedDest, String)>;
pub type TlsNameMap = HashMap<String, (Vec<IpAddr>, u16)>;

pub struct Resolver {
	pub destinations: Arc<RwLock<WellKnownMap>>, // actual_destination, host
	pub overrides: Arc<StdRwLock<TlsNameMap>>,
	pub resolver: Arc<TokioAsyncResolver>,
	pub hooked: Arc<Hooked>,
}

pub struct Hooked {
	pub overrides: Arc<StdRwLock<TlsNameMap>>,
	pub resolver: Arc<TokioAsyncResolver>,
}

impl Resolver {
	pub(crate) fn new(_config: &Config) -> Self {
		let overrides = Arc::new(StdRwLock::new(TlsNameMap::new()));
		let resolver = Arc::new(TokioAsyncResolver::tokio_from_system_conf().map_err(|e| {
			error!("Failed to set up trust dns resolver with system config: {}", e);
			Error::bad_config("Failed to set up trust dns resolver with system config.")
		})
		.unwrap());

		Resolver {
			destinations: Arc::new(RwLock::new(WellKnownMap::new())),
			overrides: overrides.clone(),
			resolver: resolver.clone(),
			hooked: Arc::new(Hooked {
				overrides,
				resolver,
			}),
		}
	}
}

impl Resolve for Resolver {
	fn resolve(&self, name: Name) -> Resolving {
		resolve_to_reqwest(self.resolver.clone(), name)
	}
}

impl Resolve for Hooked {
	fn resolve(&self, name: Name) -> Resolving {
		self.overrides
			.read()
			.unwrap()
			.get(name.as_str())
			.map(|(override_name, port)| cached_to_reqwest(override_name, *port))
			.unwrap_or_else(|| resolve_to_reqwest(self.resolver.clone(), name))
	}
}

fn cached_to_reqwest(override_name: &[IpAddr], port: u16) -> Resolving {
	override_name
		.first()
		.map(|first_name| -> Resolving {
			let saddr = SocketAddr::new(*first_name, port);
			let result: Box<dyn Iterator<Item = SocketAddr> + Send> = Box::new(iter::once(saddr));
			Box::pin(future::ready(Ok(result)))
		})
		.unwrap()
}

fn resolve_to_reqwest(resolver: Arc<TokioAsyncResolver>, name: Name) -> Resolving {
	Box::pin(async move {
		let results = resolver
			.lookup_ip(name.as_str())
			.await?
			.into_iter()
			.map(|ip| SocketAddr::new(ip, 0));

		Ok(Box::new(results) as Addrs)
	})
}