use std::io;

enum State {
   SynRcvd,
   Estab,
   FinWait1,
   FinWait2,
   TimeWait
}

impl State {
   fn is_synchronized(&self) -> bool {
      match *self {
         State::SynRcvd => false,
         State::Estab | State::FinWait1 | State::FinWait2 | State::TimeWait => true,
      }
   }
}

pub struct Connection {
   state: State,
   send: SendSequenceSpace,
   recv: RecvSequenceSpace,
   ip: etherparse::Ipv4Header,
   tcp: etherparse::TcpHeader,
}


//  Send Sequence Space (RFC793 S3.2 F4) 
//
//                   1         2          3          4
//              ----------|----------|----------|----------
//                     SND.UNA    SND.NXT    SND.UNA
//                                          +SND.WND
//
//        1 - old sequence numbers which have been acknowledged
//        2 - sequence numbers of unacknowledged data
//        3 - sequence numbers allowed for new data transmission
//        4 - future sequence numbers which are not yet allowed

pub struct SendSequenceSpace {
   /// send unacknowledged
   una: u32,
   /// send next
   nxt: u32,
   /// send window
   wnd: u16,
   /// send urgent pointer
   up:  bool,
   /// segment sequence number used for last window update
   wl1: usize,
   /// segment acknowledgment number used for last window update
   wl2: usize,
   /// initial send sequence number
   iss: u32
}

//  Receive Sequence Space (RFC793 S3.2 F5) 
//
//                       1          2          3
//                 ----------|----------|----------
//                          RCV.NXT    RCV.NXT
//                                   +RCV.WND
//
//        1 - old sequence numbers which have been acknowledged
//        2 - sequence numbers allowed for new reception
//        3 - future sequence numbers which are not yet allowed

pub struct RecvSequenceSpace {
   /// receive next
   nxt: u32,
   /// receive window
   wnd: u16,
   /// receive urgent pointer
   up: bool,
   /// initial receive sequence number
   irs: u32
}

impl Connection {
   pub fn write( &mut self, nic: &mut tun_tap::Iface, payload: &[u8]) -> io::Result<usize> {
      let mut buf = [0u8; 1500];
      self.tcp.sequence_number = self.send.nxt;
      self.tcp.acknowledgment_number = self.recv.nxt;

      let size = std::cmp::min(
         buf.len(), 
         self.tcp.header_len() as usize + self.ip.header_len() as usize + payload.len());  
      self.ip.set_payload_len(size - self.ip.header_len() as usize);
      self.tcp.checksum = self.tcp.calc_checksum_ipv4(&self.ip, &[]).expect("failed to compute checksum");

      // write out the headers
      use std::io::Write;
      let mut unwritten = &mut buf[..];
      self.ip.write(&mut unwritten);
      self.tcp.write(&mut unwritten);
      let payload_bytes = unwritten.write(payload)?;
      let unwritten = unwritten.len();
      self.send.nxt = self.send.nxt.wrapping_add(payload_bytes as u32);
      if self.tcp.syn {
         self.send.nxt = self.send.nxt.wrapping_add(1);
         self.tcp.syn = false;
      }
      if self.tcp.fin {
         self.send.nxt = self.send.nxt.wrapping_add(1);
         self.tcp.fin = false;
      }
      nic.send(&buf[.. buf.len() - unwritten])?;
      Ok(payload_bytes)
   }

   pub fn send_rst(&mut self, nic: &mut tun_tap::Iface) -> io::Result<()>{
       self.tcp.rst = true;
       self.tcp.sequence_number = 0;
       self.tcp.acknowledgment_number = 0;
       self.write(nic, &[])?; 
       Ok(())     
   }

   pub fn on_packet<'a>(
           &mut self, 
           nic: &mut tun_tap::Iface,
           iph: etherparse::Ipv4HeaderSlice<'a>,
           tcph: etherparse::TcpHeaderSlice<'a>,
           data: &'a [u8],
   ) -> io::Result<()>{
        // First check sequence numbers are valid (RFC793 S3.3)
        let seqn = tcph.sequence_number();
        let strt = self.recv.nxt.wrapping_sub(1);

        let mut slen = data.len() as u32;
        if tcph.syn() {
           slen += 1;
        }
        if tcph.fin(){
           slen += 1;
        }
 
        let wend = self.recv.nxt.wrapping_add(self.recv.wnd as u32);
        let okay = if slen == 0 {
           // zero-length segment has separate rules for acceptance
           if self.recv.wnd == 0 {
              if seqn != self.recv.nxt {
                 false
              } else {
                 true
              }
           } else if !is_between_wrapped(strt, seqn, wend){
                 false
           } else {
              true
           }
        } else {
           if self.recv.wnd == 0 {
              false
           } else if !is_between_wrapped(strt, seqn, wend) && !is_between_wrapped(strt, seqn.wrapping_add(slen-1) as u32-1 , wend){
              false
           } else {
              true
           }
        };

        if !okay {
           self.write(nic, &[])?;
           return Ok(());
        }

        self.recv.nxt = seqn.wrapping_add(slen);
        // TODO: If not acceptable send an ACK

        if !tcph.ack() {
           return Ok(());
        }

        let ackn = tcph.acknowledgment_number();
        if let State::SynRcvd = self.state {
           if is_between_wrapped(self.send.una.wrapping_sub(1), ackn, self.send.nxt.wrapping_add(1)){
             //must have ACKed our SYN, since we detected at least one acked byte, and we have only sent one byte (the SYN)
             self.state = State::Estab;
            } else {
             // TODO: <SEQ=SEG.ACK><CTL=RST>
            }
        }

         if let State::Estab | State::FinWait1 | State::FinWait2 = self.state {
              if !is_between_wrapped(self.send.una, ackn, self.send.nxt.wrapping_add(1)){
                 return Ok(());
              }
              self.send.una = ackn;
              assert!(data.is_empty());
              
              // Now lets terminate the connection!
              // TODO: needs to be stored in the retransmission queue!
              if let State::Estab = self.state {
                 self.tcp.fin = true;
                 self.write(nic, &[])?;
                 self.state = State::FinWait1;
              }
         }

         if let State::FinWait1 = self.state {
              if self.send.una == self.send.iss + 2 {
                 // our FIN has been acked
                 self.state = State::FinWait2;
              }
         }

         if tcph.fin(){
            match self.state {
               State::FinWait2 => {
                  // We are done with the connection
                  self.write(nic, &[])?;
                  self.state = State::TimeWait;
               }
               _ => unimplemented!(),
            }
         }
      
         Ok(())
    }
    pub fn accept<'a>(nic: &mut tun_tap::Iface,
           iph: etherparse::Ipv4HeaderSlice<'a>,
           tcph: etherparse::TcpHeaderSlice<'a>,
           data: &'a [u8],
    ) -> io::Result<Option<Self>>
    {
                  let mut buf = [0u8; 1500];
                  if !tcph.syn(){
                     // Only expected syn package
                     return Ok(None);
                  }

                  let iss = 0;
                  let wnd = 1024;
                  let mut c = Connection {
                     state: State::SynRcvd,
                     send: SendSequenceSpace{
                          iss,
                          una: iss,
                          nxt: iss,
                          wnd: wnd,
                          up: false,
                          wl1: 0,
                          wl2: 0,
                     },
                     recv: RecvSequenceSpace{
                          nxt: tcph.sequence_number() + 1,
                          wnd: tcph.window_size(),
                          irs: tcph.sequence_number(),
                          up: false,
                     },
                     ip: etherparse::Ipv4Header::new(
                        0,
                        64,
                        etherparse::IpTrafficClass::Tcp,
                        [
                           iph.destination()[0], iph.destination()[1],iph.destination()[2], iph.destination()[3],
                        ],
                        [
                           iph.source()[0], iph.source()[1], iph.source()[2], iph.source()[3],
                        ]
                     ),
                     tcp: etherparse::TcpHeader::new(
                        tcph.destination_port(),
                        tcph.source_port(),
                        iss,
                        wnd,
                     )
                  };

                  c.tcp.syn = true;   
                  c.tcp.ack = true;
                  c.write(nic, &[])?;
                  Ok(Some(c))
    }
}

fn is_between_wrapped(start:u32, x:u32, end: u32) -> bool {
   use std::cmp::Ordering;
   match start.cmp(&x){
      Ordering::Equal => return false,
      Ordering::Less => {
         // We have:
         //    0  |..............S.............X...............| (wraparound)
         //
         // X is between S and E (S < X < E) in these cases:
         //    0  |...........E..S.............X...............| (wraparound)
         //    0  |..............S.............X..E............| (wraparound)   
         //
         // But *not* in these cases:
         //    0  |..............S......E......X...............| (wraparound)
         //    0  |..............|.............X...............| (wraparound)
         //                     ^-S+E
         //    0  |..............S.............|...............| (wraparound)
         //                               X+E-^     
         //
         // Or in other words, iff !(S <= E <= X)
         if end >= start && end <= x {
            return false;
         }
      }

      Ordering::Greater => {
         // we have the opposite of above
         //    0  |..............X.............S...............| (wraparound)
         //
         // X is between S and E (S < X < E) *only* in these case:
         //    0  |..............X.....E.......S...............| (wraparound)
         //
         // But *not* in these cases:
         //    0  |..............X.............S..E............| (wraparound)
         //    0  |...........E..X.............S...............| (wraparound)
         //    0  |..............|.............S...............| (wraparound)
         //                     ^-X+E
         //    0  |..............X.............|...............| (wraparound)
         //                               S+E-^     
         //
         // Or in other words, iff S < E < X
         if end < start && end > x {}
         else {
            return false;
         }
      }
   }

   true
}