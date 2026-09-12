//! Bounded TLS connection lifecycle. HTTP handlers retain their own scoped
//! authentication; accepting a connection does not grant application access.

use crate::{
    Error,
    config::{self, TlsConfig},
    invalid,
};
use axum::Router;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::sync::Arc;
use zuno_engine::interrupt::InterruptSignal;

pub async fn serve(
    options: &TlsConfig,
    routes: Router,
    shutdown: InterruptSignal,
) -> Result<(), Error> {
    if !(1..=4096).contains(&options.max_connections) {
        return Err(invalid("TLS connection limit must be 1–4096"));
    }
    let certificates = config::read_file(&options.certificate_file, 65536).await?;
    let private = config::read_file(&options.private_key_file, 65536).await?;
    let certificates = CertificateDer::pem_slice_iter(&certificates)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("invalid TLS certificate chain"))?;
    let private =
        PrivateKeyDer::from_pem_slice(&private).map_err(|_| invalid("invalid TLS private key"))?;
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| invalid("invalid TLS versions"))?
    .with_no_client_auth()
    .with_single_cert(certificates, private)
    .map_err(|_| invalid("TLS certificate and private key do not match"))?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    tracing::info!(address=%listener.local_addr()?, "enterprise TLS listener started");
    let slots = Arc::new(tokio::sync::Semaphore::new(
        options.max_connections as usize,
    ));
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _=shutdown.notified()=>break,
            Some(result)=connections.join_next(),if !connections.is_empty()=>{
                if result.is_err() {tracing::warn!("enterprise connection task did not finish cleanly")}
            }
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;
                let Ok(permit)=slots.clone().try_acquire_owned() else {continue};
                let acceptor=acceptor.clone();
                let routes=routes.clone();
                let stopped=shutdown.clone();
                connections.spawn(async move {
                    let _permit=permit;
                    let stream=tokio::select! {
                        biased;
                        _=stopped.notified()=>return,
                        result=tokio::time::timeout(std::time::Duration::from_secs(5),acceptor.accept(stream))=>{
                            let Ok(Ok(stream))=result else {return};stream
                        }
                    };
                    let builder=Builder::new(TokioExecutor::new());
                    let connection=builder.serve_connection_with_upgrades(TokioIo::new(stream),TowerToHyperService::new(routes));
                    tokio::pin!(connection);
                    tokio::select! {
                        _=&mut connection=>{},
                        _=stopped.notified()=>{
                            connection.as_mut().graceful_shutdown();
                            let _=tokio::time::timeout(std::time::Duration::from_secs(30),connection).await;
                        }
                    }
                });
            }
        }
    }
    let drained = tokio::time::timeout(std::time::Duration::from_secs(31), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    Ok(())
}
