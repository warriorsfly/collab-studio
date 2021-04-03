use actix::Actor;
use actix_web::{middleware::Logger, web, App, HttpServer};
use redis::Client;

use crate::{
    act::{Redis, Websocket},
    handler::socket_route,
};

pub async fn serv() -> std::io::Result<()> {
    // Start socket server actor
    let server = Websocket::default().start();
    let cli = Client::open("redis://127.0.0.1:6379").expect("unable to connect to redis");
    let redis = Redis::new(cli).start();
    // Create Http server with websocket support
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .data(server.clone())
            .data(redis.clone())
            .service(web::resource("/notify/").to(socket_route))
        // static resources
        // .service(fs::Files::new("/static/", "static/"))
    })
    .bind("127.0.0.1:3000")?
    .run()
    .await
}