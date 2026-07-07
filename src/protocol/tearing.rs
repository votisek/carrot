// wp-tearing-control-v1. the hint is per-surface, double-buffered, and only
// consulted for the fullscreen surface at present time - vsync stays the
// default everywhere else.

use crate::client::{Client, ClientError, Object};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{wp_tearing_control_manager_v1, wp_tearing_control_v1};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::surface::WlSurface;
use std::rc::Rc;

const HINT_VSYNC: u32 = 0;
const HINT_ASYNC: u32 = 1;
const ERR_CONTROL_EXISTS: u32 = 0;

pub struct TearingManagerGlobal;

impl Global for TearingManagerGlobal {
    fn interface(&self) -> &'static str {
        wp_tearing_control_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        1
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(TearingManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct TearingManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl wp_tearing_control_manager_v1::Handler for TearingManager {
    fn destroy(
        &self,
        _req: wp_tearing_control_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }

    fn get_tearing_control(
        &self,
        req: wp_tearing_control_manager_v1::get_tearing_control::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let c = &self.client;
        let Some(surface) = c.objects.surface(req.surface) else {
            c.invalid_object(req.surface);
            return Ok(());
        };
        if surface.tearing_control.get() {
            c.protocol_error(
                self.id,
                ERR_CONTROL_EXISTS,
                "the surface already has a tearing control",
            );
            return Ok(());
        }
        surface.tearing_control.set(true);
        c.add_client_obj(Rc::new(TearingControl {
            id: req.id,
            client: c.clone(),
            version: self.version,
            surface,
        }))?;
        Ok(())
    }
}

impl Object for TearingManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wp_tearing_control_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wp_tearing_control_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

pub struct TearingControl {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
    pub surface: Rc<WlSurface>,
}

impl wp_tearing_control_v1::Handler for TearingControl {
    fn set_presentation_hint(
        &self,
        req: wp_tearing_control_v1::set_presentation_hint::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let hint = match req.hint {
            HINT_VSYNC => false,
            HINT_ASYNC => true,
            other => {
                self.client.protocol_error(
                    self.id,
                    ERR_CONTROL_EXISTS,
                    &format!("{other} is not a presentation hint"),
                );
                return Ok(());
            }
        };
        self.surface.pending.borrow_mut().tearing = Some(hint);
        Ok(())
    }

    fn destroy(
        &self,
        _req: wp_tearing_control_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // per spec the hint reverts to vsync, applied on the next commit
        self.surface.pending.borrow_mut().tearing = Some(false);
        self.surface.tearing_control.set(false);
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for TearingControl {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        wp_tearing_control_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        wp_tearing_control_v1::dispatch(&*self, self.version, opcode, r)
    }

    fn break_loops(&self) {
        self.surface.tearing_control.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use wp_tearing_control_manager_v1::Handler as _;
    use wp_tearing_control_v1::Handler as _;

    fn setup() -> (Rc<Client>, Rc<WlSurface>, Rc<TearingManager>) {
        let (_state, client) = test_client();
        let s = WlSurface::new(ObjectId(10), &client, 6);
        client.add_client_obj(s.clone()).unwrap();
        client.objects.track_surface(s.clone());
        let mgr = Rc::new(TearingManager {
            id: ObjectId(60),
            client: client.clone(),
            version: 1,
        });
        client.add_client_obj(mgr.clone()).unwrap();
        (client, s, mgr)
    }

    fn wl_display_errors(client: &Rc<Client>) -> usize {
        count_events(&client.queued_out_bytes(), ObjectId(1), 0)
    }

    #[test]
    fn the_hint_is_double_buffered_and_destroy_reverts() {
        let (client, s, mgr) = setup();
        mgr.get_tearing_control(wp_tearing_control_manager_v1::get_tearing_control::Request {
            id: ObjectId(61),
            surface: s.id,
        })
        .unwrap();
        let ctl = Rc::new(TearingControl {
            id: ObjectId(61),
            client: client.clone(),
            version: 1,
            surface: s.clone(),
        });
        ctl.set_presentation_hint(wp_tearing_control_v1::set_presentation_hint::Request {
            hint: HINT_ASYNC,
        })
        .unwrap();
        // pending until the commit
        assert!(!s.tearing.get());
        s.commit_impl();
        assert!(s.tearing.get());
        ctl.destroy(wp_tearing_control_v1::destroy::Request {}).unwrap();
        assert!(s.tearing.get(), "revert waits for the commit");
        s.commit_impl();
        assert!(!s.tearing.get());
        assert!(!s.tearing_control.get());
    }

    #[test]
    fn one_control_per_surface() {
        let (client, s, mgr) = setup();
        mgr.get_tearing_control(wp_tearing_control_manager_v1::get_tearing_control::Request {
            id: ObjectId(61),
            surface: s.id,
        })
        .unwrap();
        assert_eq!(wl_display_errors(&client), 0);
        mgr.get_tearing_control(wp_tearing_control_manager_v1::get_tearing_control::Request {
            id: ObjectId(62),
            surface: s.id,
        })
        .unwrap();
        // the second control is a protocol error
        assert_eq!(wl_display_errors(&client), 1);
    }
}
