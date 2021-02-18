use smoltcp::wire::IpAddress as Address;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

pub fn convert_to_socket_address(
    address: Address,
    port: u16,
) -> Result<SocketAddr, smoltcp::Error> {
    let socket_address = match address {
        Address::Ipv4(v4) => SocketAddr::from((v4.0, port)),
        Address::Ipv6(v6) => {
            let mut b = [0u16; 8];
            v6.write_parts(&mut b);
            SocketAddr::from((b, port))
        }
        _ => return Err(smoltcp::Error::Unrecognized),
    };
    Ok(socket_address)
}

pub(crate) struct Selector<A, B, C, D> {
    a: Pin<Box<Option<A>>>,
    b: Pin<Box<Option<B>>>,
    c: Pin<Box<Option<C>>>,
    d: Pin<Box<Option<D>>>,
}

impl<A, B, C, D> Selector<A, B, C, D> {
    pub fn new() -> Self {
        Selector {
            a: Box::pin(None),
            b: Box::pin(None),
            c: Box::pin(None),
            d: Box::pin(None),
        }
    }
    pub fn b(&self) -> &Option<B> {
        self.b.as_ref().get_ref()
    }
    pub fn insert_a(&mut self, a: A) {
        assert!(self.a.is_none());
        self.a.set(Some(a));
    }
    pub fn insert_b(&mut self, b: B) {
        assert!(self.b.is_none());
        self.b.set(Some(b));
    }
    pub fn insert_c(&mut self, c: C) {
        assert!(self.c.is_none());
        self.c.set(Some(c));
    }
    pub fn insert_d(&mut self, d: D) {
        assert!(self.d.is_none());
        self.d.set(Some(d));
    }
}

pub(crate) struct SelectorFuture<'a, A, B, C, D> {
    select: &'a mut Selector<A, B, C, D>,
}

impl<A, B, C, D> Selector<A, B, C, D>
where
    A: Future,
    B: Future,
    C: Future,
    D: Future,
{
    pub(crate) fn select(&mut self) -> SelectorFuture<A, B, C, D> {
        SelectorFuture { select: self }
    }
}

impl<A, B, C, D> Future for SelectorFuture<'_, A, B, C, D>
where
    A: Future,
    B: Future,
    C: Future,
    D: Future,
{
    type Output = Selected<A::Output, B::Output, C::Output, D::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sel = &mut self.select;
        let a = &mut sel.a;
        let m = a.as_mut();
        let p = m.as_pin_mut();
        if let Some(b) = p {
            if let Poll::Ready(x) = b.poll(cx) {
                sel.a.set(None);
                return Poll::Ready(Selected::A(x));
            }
        }
        if let Some(b) = sel.b.as_mut().as_pin_mut() {
            if let Poll::Ready(x) = b.poll(cx) {
                sel.b.set(None);
                return Poll::Ready(Selected::B(x));
            }
        }
        let c = &mut sel.c;
        if let Some(b) = c.as_mut().as_pin_mut() {
            if let Poll::Ready(x) = b.poll(cx) {
                c.set(None);
                return Poll::Ready(Selected::C(x));
            }
        }
        let c = &mut sel.d;
        if let Some(b) = c.as_mut().as_pin_mut() {
            if let Poll::Ready(x) = b.poll(cx) {
                c.set(None);
                return Poll::Ready(Selected::D(x));
            }
        }
        Poll::Pending
    }
}

#[derive(Debug)]
pub(crate) enum Selected<A, B, C, D> {
    A(A),
    B(B),
    C(C),
    D(D),
}

#[cfg(test)]
mod tests {
    use crate::util::{Selected, Selector};
    use tokio::time::{sleep, Duration};

    async fn fut2(u: u8) -> String {
        sleep(Duration::from_millis(1)).await;
        format!("u{}", u + 1)
    }
    #[tokio::test]
    async fn testss() {
        let future1 = async {
            sleep(Duration::from_millis(3)).await;
            "a"
        };
        let future2 = fut2(34);
        let future3 = async {
            sleep(Duration::from_millis(3)).await;
            "a"
        };
        let mut s = Selector::new();
        s.insert_a(future1);
        s.insert_b(future2);
        s.insert_c(future3);
        s.insert_d(async {
            sleep(Duration::from_millis(9)).await;
            1
        });
        {
            let f = s.select();
            let o = f.await;
            match o {
                Selected::A(x) => panic!("Output is {:?}", x),
                Selected::B(n) => assert_eq!("u35", n),
                Selected::C(_) => {}
                Selected::D(_) => {}
            }
        }
        {
            let f = s.select();
            let o = f.await;
            match o {
                Selected::A(x) => assert_eq!("a", x),
                Selected::B(n) => panic!("Output is {:?}", n),
                Selected::C(_) => {}
                _ => {}
            }
        }
        let future3 = fut2(75);
        s.insert_b(future3);
        let _o = s.select().await;
        let _o = s.select().await;
    }
}
