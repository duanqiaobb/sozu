#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::tcp::*;
use std::io::{self,Read,ErrorKind};
use mio::*;
use mio::buf::{ByteBuf,MutByteBuf};
use std::collections::HashMap;
use nom::{HexDisplay,IResult};
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::{FromStr, from_utf8};
use time::precise_time_s;

use parser::http11::{RRequestLine,RequestHeader,request_line,headers};

use messages::Command;

pub type Host = String;

#[derive(Debug,Clone)]
pub enum ErrorState {
  InvalidHttp,
  MissingHost
}

#[derive(Debug,Clone)]
pub enum LengthInformation {
  Length(usize),
  Chunked,
  Compressed
}

type BackendToken = Token;
#[derive(Debug,Clone)]
pub enum HttpState {
  Initial,
  Error(ErrorState),
  HasRequestLine(usize, RRequestLine),
  HasHost(usize, RRequestLine, Host),
  HeadersParsed(RRequestLine, Host, LengthInformation),
  Proxying(RRequestLine, Host, LengthInformation, BackendToken)
}


#[derive(Debug)]
pub enum HttpProxyOrder {
  Command(Command),
  Stop
}

#[derive(Debug)]
pub enum ServerMessage {
  AddedHttpFront,
  RemovedHttpFront,
  AddedInstance,
  RemovedInstance,
  Stopped
}


struct Client {
  sock:           TcpStream,
  backend:        Option<TcpStream>,
  http_state:     HttpState,
  front_buf:      Option<ByteBuf>,
  front_mut_buf:  Option<MutByteBuf>,
  back_buf:       Option<ByteBuf>,
  back_mut_buf:   Option<MutByteBuf>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  back_interest:  EventSet,
  front_interest: EventSet,
  rx_count:       usize,
  tx_count:       usize
}

impl Client {
  fn new(sock: TcpStream) -> Option<Client> {
    Some(Client {
      sock:           sock,
      backend:        None,
      http_state:     HttpState::Initial,
      front_buf:      None,
      front_mut_buf:  Some(ByteBuf::mut_with_capacity(2048)),
      back_buf:       None,
      back_mut_buf:   Some(ByteBuf::mut_with_capacity(2048)),
      token:          None,
      backend_token:  None,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      rx_count:       0,
      tx_count:       0
    })
  }

  pub fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  pub fn close(&self) {
    // ToDo close sockets and remove from slabs
  }

  fn parse_headers(state: &HttpState, buf: &MutByteBuf) -> HttpState {
    match state {
      &HttpState::Initial => {
        println!("buf: {}", buf.bytes().to_hex(8));
        match request_line(buf.bytes()) {
          IResult::Error(e) => {
            println!("error: {:?}", e);
            HttpState::Error(ErrorState::InvalidHttp)
          },
          IResult::Incomplete(_) => {
            state.clone()
          },
          IResult::Done(i, r)    => {
            if let Some(rl) = RRequestLine::fromRequestLine(r) {
              let s = HttpState::HasRequestLine(buf.bytes().offset(i), rl);
              println!("now in state: {:?}", s);
              Client::parse_headers(&s, buf)
            } else {
              HttpState::Error(ErrorState::InvalidHttp)
            }
          }
        }
      },
      &HttpState::HasRequestLine(pos, ref rl) => {
        println!("parsing headers from:\n{}", (&buf.bytes()[pos..]).to_hex(8));
        match headers(&buf.bytes()[pos..]) {
          IResult::Error(e) => {
            println!("error: {:?}", e);
            HttpState::Error(ErrorState::InvalidHttp)
          },
          IResult::Incomplete(_) => {
            state.clone()
          },
          IResult::Done(i, v)    => {
            println!("got headers: {:?}", v);
            for header in v.iter() {
              if from_utf8(header.name) == Ok("Host") {
                if let Ok(host) = from_utf8(header.value) {
                  return HttpState::HasHost(buf.bytes().offset(i), rl.clone(), String::from(host));
                } else {
                  return HttpState::Error(ErrorState::InvalidHttp);
                }
              }
            }
            HttpState::HasRequestLine(buf.bytes().offset(i), rl.clone())
         }
        }
      },
      //HasHost(usize,RRequestLine, Host),
      _ => {
        panic!("unimplemented state: {:?}", state);
      }
    }
  }

  // Forward content to client
  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    Ok(())
    // ToDo
    /*
    //println!("in writable()");
    if let Some(mut buf) = self.back_buf.take() {
      //println!("in writable 2: back_buf contains {} bytes", buf.remaining());

      match self.sock.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.back_buf = Some(buf);
          self.front_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          //println!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.back_mut_buf = Some(buf.flip());
          self.tx_count = self.tx_count + r;

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
    }
    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
    */
  }

  fn is_proxying(&self) -> bool {
    if let HttpState::HasHost(_, _, _) = self.http_state {
      true
    } else {
      false
    }
  }

  fn flip_front_buf(&mut self, buf: MutByteBuf, event_loop: &mut EventLoop<Server>) {
    self.front_interest.remove(EventSet::readable());
    self.back_interest.insert(EventSet::writable());
    // prepare to provide this to writable
    self.front_buf = Some(buf.flip());
    //event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
  }

  // Read content from the client
  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    println!("in readable()");
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    let mut buf = self.front_mut_buf.take().unwrap();
    self.sock.try_read_buf(&mut buf).map(|res| {
      if let Some(r) = res {
        println!("FRONT [{:?}]: read {} bytes", self.token, r);
        if self.is_proxying() {
          //println!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
          self.flip_front_buf(buf, event_loop);
          self.rx_count = self.rx_count + r;
        } else {
          let state = Client::parse_headers(&self.http_state, &buf);
          if let HttpState::Error(_) = state {
            self.front_mut_buf = Some(buf);
            self.close();
            self.http_state = state;
            return;
          }
          self.http_state = state;
          println!("new state: {:?}", self.http_state);
          if self.is_proxying() {
            self.rx_count = buf.remaining();
            self.flip_front_buf(buf, event_loop);
            println!("is now proxying, front buf flipped");
          //if let HttpState::HasHost(i, ref rl, ref host) = self.http_state {
          //  self.rx_count = buf.remaining();
          //  self.flip_front_buf(buf, event_loop);
          } else {
            self.front_mut_buf = Some(buf);
            println!("TOKEN: {:?}", self.token);
            self.front_interest.insert(EventSet::readable());
            event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
            println!("AAA");
          }
        }
      }
    });
    Ok(())
  }

  // Forward content to application
  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    Ok(())
    // ToDo
    /*
    if let Some(mut buf) = self.front_buf.take() {
      //println!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

      match self.backend.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.front_buf = Some(buf);
          self.back_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          //println!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.front_mut_buf = Some(buf.flip());

          self.front_interest.insert(EventSet::readable());
          self.back_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
    }
    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
    */
  }

  // Read content from application
  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    Ok(())
    // ToDo
    /*
    let mut buf = self.back_mut_buf.take().unwrap();
    //println!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

    match self.backend.try_read_buf(&mut buf) {
      Ok(None) => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        //println!("BACK  [{}<-{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.back_interest.remove(EventSet::readable());
        self.front_interest.insert(EventSet::writable());
        // prepare to provide this to writable
        self.back_buf = Some(buf.flip());
      }
      Err(e) => {
        println!("not implemented; client err={:?}", e);
        //self.interest.remove(EventSet::readable());
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
    */
  }
}


pub struct ApplicationListener {
  sock:           TcpListener,
  token:          Token,
  front_address:  SocketAddr
}

type ClientToken = Token;

pub struct Server {
  instances:       HashMap<String, Vec<SocketAddr>>,
  listener:        ApplicationListener,
  clients:         Slab<Client>,
  backend:         Slab<ClientToken>,
  max_listeners:   usize,
  max_connections: usize,
  tx:              mpsc::Sender<ServerMessage>
}

impl Server {
  fn new(listener: ApplicationListener, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> Server {
    Server {
      instances:       HashMap::new(),
      listener:        listener,
      clients:         Slab::new_starting_at(Token(1), max_connections),
      backend:         Slab::new_starting_at(Token(1 + max_connections), max_connections),
      max_listeners:   1,
      max_connections: max_connections,
      tx:              tx
    }
  }

  //pub fn add_tcp_front(&mut self, port: u16, app_id: &str, event_loop: &mut EventLoop<Server>) -> Option<Token> {
  //  let addr_string = String::from("127.0.0.1:") + &port.to_string();
  //  let front = &addr_string.parse().unwrap();

  //  if let Ok(listener) = TcpListener::bind(front) {
  //    let addresses = if let Some(ads) = self.instances.get(app_id) {
  //      ads.clone()
  //    } else {
  //      Vec::new()
  //    };

  //    let al = ApplicationListener {
  //      app_id:         String::from(app_id),
  //      sock:           listener,
  //      token:          None,
  //      front_address:  *front,
  //      back_addresses: addresses
  //    };

  //    if let Ok(tok) = self.listeners.insert(al) {
  //      self.listeners[tok].token = Some(tok);
  //      self.fronts.insert(String::from(app_id), tok);
  //      event_loop.register_opt(&self.listeners[tok].sock, tok, EventSet::readable(), PollOpt::level()).unwrap();
  //      println!("registered listener for app {} on port {}", app_id, port);
  //      Some(tok)
  //    } else {
  //      println!("could not register listener for app {} on port {}", app_id, port);
  //      None
  //    }

  //  } else {
  //    println!("could not declare listener for app {} on port {}", app_id, port);
  //    None
  //  }
  //}

  //pub fn remove_tcp_front(&mut self, app_id: String, event_loop: &mut EventLoop<Server>) -> Option<Token>{
  //  println!("removing tcp_front {:?}", app_id);
  //  // ToDo
  //  // Removes all listeners for the given app_id
  //  // an app can't have two listeners. Is this a problem?
  //  if let Some(&tok) = self.fronts.get(&app_id) {
  //    if self.listeners.contains(tok) {
  //      event_loop.deregister(&self.listeners[tok].sock);
  //      self.listeners.remove(tok);
  //      println!("removed server {:?}", tok);
  //      //self.listeners[tok].sock.shutdown(Shutdown::Both);
  //      Some(tok)
  //    } else {
  //      None
  //    }
  //  } else {
  //    None
  //  }
  //}

  //pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) -> Option<Token> {
  //  if let Some(addrs) = self.instances.get_mut(app_id) {
  //      addrs.push(*instance_address);
  //  }

  //  if self.instances.get(app_id).is_none() {
  //    self.instances.insert(String::from(app_id), vec![*instance_address]);
  //  }

  //  if let Some(&tok) = self.fronts.get(app_id) {
  //    let application_listener = &mut self.listeners[tok];

  //    application_listener.back_addresses.push(*instance_address);
  //    Some(tok)
  //  } else {
  //    println!("No front for this instance");
  //    None
  //  }
  //}

  //pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) -> Option<Token>{
  //    // ToDo
  //    None
  //}

  pub fn accept(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    let application_listener = &self.listener;
    let accepted = application_listener.sock.accept();

    if let Ok(Some(frontend_sock)) = accepted {
      if let Some(client) = Client::new(frontend_sock) {
        if let Ok(client_token) = self.clients.insert(client) {
            event_loop.register_opt(&self.clients[client_token].sock, client_token, EventSet::readable(), PollOpt::edge()).unwrap();
            self.clients[client_token].set_front_token(client_token);
        } else {
          println!("could not add client to slab");
        }
      } else {
        println!("could not create a client");
      }
    } else {
      println!("could not accept connection: {:?}", accepted);
    }
  }
}

impl Handler for Server {
  type Timeout = usize;
  type Message = HttpProxyOrder;

  fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
    println!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      //println!("{:?} is readable", token);
      if token == Token(0) {
        self.accept(event_loop, token)
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].readable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_readable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
      //match token {
      //  SERVER => self.server.accept(event_loop).unwrap(),
      //  i => self.server.conn_readable(event_loop, i).unwrap()
     // }
    }

    if events.is_writable() {
      //println!("{:?} is writable", token);
      if token.as_usize() < self.max_listeners {
        println!("received writable for listener {:?}, this should not happen", token);
      } else  if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].writable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_writable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
      //match token {
      //  SERVER => panic!("received writable for token 0"),
        //CLIENT => self.client.writable(event_loop).unwrap(),
      //  _ => self.server.conn_writable(event_loop, token).unwrap()
      //};
    }

    if events.is_hup() {
      if token == Token(0) {
        println!("should not happen: server {:?} closed", token);
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          println!("removing client {:?}", token);
          self.clients[token].close();
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            println!("removing client {:?}", tok);
            self.clients[tok].close();
          } else {
            println!("client {:?} was removed", token);
          }
        } else {

          println!("backend {:?} was removed", token);
        }

      }
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
  // ToDo temporary
  //  println!("notified: {:?}", message);
  //  match message {
  //    TcpProxyOrder::Command(Command::AddTcpFront(front)) => {
  //      println!("{:?}", front);
  //      if let Some(token) = self.add_tcp_front(front.port, &front.app_id, event_loop) {
  //        self.tx.send(ServerMessage::AddedTcpFront);
  //      } else {
  //        println!("Couldn't add tcp front");
  //      }
  //    },
  //    TcpProxyOrder::Command(Command::RemoveTcpFront(front)) => {
  //      println!("{:?}", front);
  //      let _ = self.remove_tcp_front(front.app_id, event_loop);
  //      self.tx.send(ServerMessage::RemovedTcpFront);
  //    },
  //    TcpProxyOrder::Command(Command::AddInstance(instance)) => {
  //      println!("{:?}", instance);
  //      let addr_string = instance.ip_address + ":" + &instance.port.to_string();
  //      let addr = &addr_string.parse().unwrap();
  //      if let Some(token) = self.add_instance(&instance.app_id, addr, event_loop) {
  //        self.tx.send(ServerMessage::AddedInstance);
  //      } else {
  //        println!("Couldn't add tcp front");
  //      }
  //    },
  //    TcpProxyOrder::Command(Command::RemoveInstance(instance)) => {
  //      println!("{:?}", instance);
  //      let addr_string = instance.ip_address + ":" + &instance.port.to_string();
  //      let addr = &addr_string.parse().unwrap();
  //      if let Some(token) = self.remove_instance(&instance.app_id, addr, event_loop) {
  //        self.tx.send(ServerMessage::RemovedInstance);
  //      } else {
  //        println!("Couldn't add tcp front");
  //      }
  //    },
  //    TcpProxyOrder::Stop                   => {
  //      event_loop.shutdown();
  //    },
  //    _ => {
  //      println!("unsupported message, ignoring");
  //    }
  //  }
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    println!("timeout");
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    println!("interrupted");
  }
}

pub fn start() {
  // ToDo temporary
  let mut event_loop = EventLoop::new().unwrap();

  let (tx,rx) = channel::<ServerMessage>();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();
  let front: SocketAddr = FromStr::from_str("127.0.0.1:8080").unwrap();

  let tcp_listener = TcpListener::bind(&front).unwrap();
  let listener = ApplicationListener {
    sock:           tcp_listener,
    token:          Token(0),
    front_address:  front
  };

  event_loop.register_opt(&listener.sock, listener.token, EventSet::readable(), PollOpt::edge()).unwrap();

  let mut server = Server::new(listener, 500, tx);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });


  //println!("listen for connections");
  //event_loop.register_opt(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  //let mut s = Server::new(10, 500, tx);
  //{
  //  let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
  //  s.add_tcp_front(1234, "yolo", &mut event_loop);
  //  s.add_instance("yolo", &back, &mut event_loop);
  //}
  //{
  //  let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
  //  s.add_tcp_front(1235, "yolo", &mut event_loop);
  //  s.add_instance("yolo", &back, &mut event_loop);
  //}
  //thread::spawn(move|| {
  //  println!("starting event loop");
  //  event_loop.run(&mut s).unwrap();
  //  println!("ending event loop");
  //});
}

pub fn start_listener(front: SocketAddr, max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<HttpProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let tcp_listener = TcpListener::bind(&front).unwrap();
  let listener = ApplicationListener {
    sock:           tcp_listener,
    token:          Token(0),
    front_address:  front
  };

  event_loop.register_opt(&listener.sock, listener.token, EventSet::readable(), PollOpt::edge()).unwrap();

  let mut server = Server::new(listener, max_connections, tx);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
}

#[cfg(test)]
mod tests {
  extern crate hyper;
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use self::hyper::Client;
  use self::hyper::header::Connection;

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let mut client = Client::new();

    // Creating an outgoing request.
    let mut res = client.get("http://localhost:8080/")
        // set a header
        .header(Connection::close())
        // let 'er go!
        .send().unwrap();

    // Read the Response.
    let mut body = String::new();
    res.read_to_string(&mut body).unwrap();

    println!("Response: {}", body);

    thread::sleep_ms(500);
    assert!(false);
  }

  use self::hyper::server::Request;
  use self::hyper::server::Response;
  use self::hyper::net::Fresh;

  fn hello(_: Request, res: Response<Fresh>) {
      res.send(b"Hello World!").unwrap();
  }

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    thread::spawn(move|| {
      hyper::Server::http("127.0.0.1:5678").unwrap().handle(hello);
    });
  }

}
