// wlr-data-control: clipboard access without a surface, for copy tools and
// clipboard managers. otherwise they map a throwaway toplevel to grab focus
// per copy, retiling the workspace. sources reuse the SelectionSource slots
// wl_data_device and primary already use; devices watch both selections.

use crate::client::{Client, ClientError, ClientId, Object};
use crate::protocol::data_device::{SelectionSource, same_source};
use crate::protocol::globals::Global;
use crate::protocol::interfaces::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};
use crate::protocol::wire::MsgReader;
use crate::protocol::{DispatchError, ObjectId};
use crate::state::State;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

// -- seat-side registry --

#[derive(Default)]
pub struct DataControl {
    devices: RefCell<Vec<Rc<DataControlDevice>>>,
    sources: RefCell<HashMap<(ClientId, u32), Rc<DataControlSource>>>,
}

impl DataControl {
    pub fn drop_client(&self, id: ClientId) {
        self.devices.borrow_mut().retain(|d| d.client.id != id);
        self.sources.borrow_mut().retain(|k, _| k.0 != id);
    }

    pub fn clear(&self) {
        self.devices.borrow_mut().clear();
        self.sources.borrow_mut().clear();
    }
}

fn seat(state: &Rc<State>) -> Option<Rc<crate::input::seat::SeatGlobal>> {
    state.seat.borrow().clone()
}

/// one of the two selections moved; every watcher gets a fresh offer
pub fn selection_changed(state: &Rc<State>, primary: bool) {
    let Some(seat) = seat(state) else { return };
    let devices = seat.data_control.devices.borrow().clone();
    for dev in devices {
        dev.send_selection(state, primary);
    }
}

// -- the manager global --

pub struct DataControlManagerGlobal;

impl Global for DataControlManagerGlobal {
    fn interface(&self) -> &'static str {
        zwlr_data_control_manager_v1::NAME
    }

    fn version(&self) -> u32 {
        2
    }

    fn bind(&self, client: &Rc<Client>, id: ObjectId, version: u32) -> Result<(), ClientError> {
        client.add_client_obj(Rc::new(DataControlManager {
            id,
            client: client.clone(),
            version,
        }))
    }
}

pub struct DataControlManager {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl zwlr_data_control_manager_v1::Handler for DataControlManager {
    fn create_data_source(
        &self,
        req: zwlr_data_control_manager_v1::create_data_source::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let src = Rc::new(DataControlSource {
            id: req.id,
            client: self.client.clone(),
            mimes: RefCell::new(Vec::new()),
        });
        self.client.add_client_obj(src.clone())?;
        if let Some(seat) = seat(&self.client.state) {
            seat.data_control
                .sources
                .borrow_mut()
                .insert((self.client.id, req.id.0), src);
        }
        Ok(())
    }

    fn get_data_device(
        &self,
        req: zwlr_data_control_manager_v1::get_data_device::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dev = Rc::new(DataControlDevice {
            id: req.id,
            client: self.client.clone(),
            version: self.version,
        });
        self.client.add_client_obj(dev.clone())?;
        if let Some(seat) = seat(&self.client.state) {
            seat.data_control.devices.borrow_mut().push(dev.clone());
        }
        // a fresh watcher learns what both clipboards hold right away
        dev.send_selection(&self.client.state, false);
        dev.send_selection(&self.client.state, true);
        Ok(())
    }

    fn destroy(
        &self,
        _req: zwlr_data_control_manager_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for DataControlManager {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_data_control_manager_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_data_control_manager_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the device (selection watcher + setter) --

pub struct DataControlDevice {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub version: u32,
}

impl DataControlDevice {
    /// current selection as a fresh offer; null when empty
    fn send_selection(&self, state: &Rc<State>, primary: bool) {
        let Some(seat) = seat(state) else { return };
        if primary && self.version < zwlr_data_control_device_v1::primary_selection::SINCE {
            return;
        }
        let src = if primary {
            seat.primary.current_source()
        } else {
            seat.data.current_source()
        };
        let offer_id = match &src {
            Some(s) => {
                let id = self.client.objects.alloc_server_id();
                let offer = Rc::new(DataControlOffer {
                    id,
                    client: self.client.clone(),
                    source: Rc::downgrade(s),
                });
                self.client.add_server_obj(offer);
                self.client.event(|o| {
                    zwlr_data_control_device_v1::data_offer::send(o, self.id, id);
                    for mime in s.mimes() {
                        zwlr_data_control_offer_v1::offer::send(o, id, &mime);
                    }
                });
                id
            }
            None => ObjectId::NONE,
        };
        self.client.event(|o| {
            if primary {
                zwlr_data_control_device_v1::primary_selection::send(o, self.id, offer_id);
            } else {
                zwlr_data_control_device_v1::selection::send(o, self.id, offer_id);
            }
        });
    }

    fn resolve_source(&self, id: ObjectId) -> Option<Option<Rc<dyn SelectionSource>>> {
        // outer None = bad id (protocol error), inner None = clear
        if id == ObjectId::NONE {
            return Some(None);
        }
        let seat = seat(&self.client.state)?;
        let src = seat
            .data_control
            .sources
            .borrow()
            .get(&(self.client.id, id.0))
            .cloned();
        src.map(|s| Some(s as Rc<dyn SelectionSource>))
    }
}

impl zwlr_data_control_device_v1::Handler for DataControlDevice {
    fn set_selection(
        &self,
        req: zwlr_data_control_device_v1::set_selection::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(src) = self.resolve_source(req.source) else {
            self.client.invalid_object(req.source);
            return Ok(());
        };
        let state = &self.client.state;
        if let Some(seat) = seat(state) {
            seat.data.set_selection_source(state, src);
            // the x bridge mirrors wl-side clipboard changes
            let xw = state.xwayland.borrow().clone();
            if let Some(xw) = xw {
                xw.queue
                    .push(crate::xwayland::XwmEvent::WlSelection { primary: false });
            }
        }
        Ok(())
    }

    fn set_primary_selection(
        &self,
        req: zwlr_data_control_device_v1::set_primary_selection::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(src) = self.resolve_source(req.source) else {
            self.client.invalid_object(req.source);
            return Ok(());
        };
        let state = &self.client.state;
        if let Some(seat) = seat(state) {
            seat.primary.set_selection_source(state, src);
            let xw = state.xwayland.borrow().clone();
            if let Some(xw) = xw {
                xw.queue
                    .push(crate::xwayland::XwmEvent::WlSelection { primary: true });
            }
        }
        Ok(())
    }

    fn destroy(
        &self,
        _req: zwlr_data_control_device_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seat) = seat(&self.client.state) {
            seat.data_control
                .devices
                .borrow_mut()
                .retain(|d| !(d.id == self.id && d.client.id == self.client.id));
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for DataControlDevice {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_data_control_device_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_data_control_device_v1::dispatch(&*self, self.version, opcode, r)
    }
}

// -- the source --

pub struct DataControlSource {
    pub id: ObjectId,
    pub client: Rc<Client>,
    pub mimes: RefCell<Vec<String>>,
}

impl SelectionSource for DataControlSource {
    fn mimes(&self) -> Vec<String> {
        self.mimes.borrow().clone()
    }

    fn send(&self, mime: &str, fd: std::os::fd::OwnedFd) {
        let fd = Rc::new(fd);
        self.client
            .event(|o| zwlr_data_control_source_v1::send::send(o, self.id, mime, fd));
    }

    fn cancelled(&self) {
        self.client
            .event(|o| zwlr_data_control_source_v1::cancelled::send(o, self.id));
    }
}

impl zwlr_data_control_source_v1::Handler for DataControlSource {
    fn offer(
        &self,
        req: zwlr_data_control_source_v1::offer::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.mimes.borrow_mut().push(req.mime_type.to_string());
        Ok(())
    }

    fn destroy(
        &self,
        _req: zwlr_data_control_source_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(seat) = seat(&self.client.state) {
            let removed = seat
                .data_control
                .sources
                .borrow_mut()
                .remove(&(self.client.id, self.id.0));
            // destroying the live selection clears it, either slot
            if let Some(r) = removed {
                let is_sel = seat
                    .data
                    .current_source()
                    .is_some_and(|s| same_source(&s, &r));
                if is_sel {
                    seat.data.set_selection_source(&self.client.state, None);
                }
                let is_primary = seat
                    .primary
                    .current_source()
                    .is_some_and(|s| same_source(&s, &r));
                if is_primary {
                    seat.primary.set_selection_source(&self.client.state, None);
                }
            }
        }
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for DataControlSource {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_data_control_source_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_data_control_source_v1::dispatch(&*self, 1, opcode, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_utils::{count_events, test_client};
    use crate::protocol::MIN_SERVER_ID;
    use zwlr_data_control_device_v1::Handler as _;
    use zwlr_data_control_manager_v1::Handler as _;
    use zwlr_data_control_source_v1::Handler as _;

    #[test]
    fn windowless_copy_sets_the_selection_and_watchers_hear_it() {
        let (state, client) = test_client();
        let seat_g = crate::input::seat::SeatGlobal::new().unwrap();
        *state.seat.borrow_mut() = Some(seat_g.clone());
        let mgr = Rc::new(DataControlManager {
            id: ObjectId(60),
            client: client.clone(),
            version: 2,
        });
        client.add_client_obj(mgr.clone()).unwrap();
        // the watcher device exists first (clipboard-manager style)
        mgr.get_data_device(zwlr_data_control_manager_v1::get_data_device::Request {
            id: ObjectId(61),
            seat: ObjectId(9),
        })
        .unwrap();
        let dev = seat_g.data_control.devices.borrow()[0].clone();
        let bytes = client.queued_out_bytes();
        // empty clipboard: null selection + null primary on creation
        assert_eq!(count_events(&bytes, dev.id, 1), 1, "initial selection");
        assert_eq!(count_events(&bytes, dev.id, 3), 1, "initial primary");
        // windowless copy: source + mime + set_selection, no toplevel anywhere
        mgr.create_data_source(zwlr_data_control_manager_v1::create_data_source::Request {
            id: ObjectId(62),
        })
        .unwrap();
        let src = seat_g
            .data_control
            .sources
            .borrow()
            .get(&(client.id, 62))
            .cloned()
            .unwrap();
        src.offer(zwlr_data_control_source_v1::offer::Request {
            mime_type: "text/plain;charset=utf-8".to_string(),
        })
        .unwrap();
        dev.set_selection(zwlr_data_control_device_v1::set_selection::Request {
            source: ObjectId(62),
        })
        .unwrap();
        assert!(seat_g.data.current_source().is_some());
        let bytes = client.queued_out_bytes();
        // the watcher got a fresh offer with the mime and a selection
        let offer_id = ObjectId(MIN_SERVER_ID);
        assert_eq!(count_events(&bytes, dev.id, 0), 1, "data_offer");
        assert_eq!(count_events(&bytes, offer_id, 0), 1, "offer(mime)");
        assert_eq!(count_events(&bytes, dev.id, 1), 2, "selection again");
        // receive pipes through to the source
        let offer = Rc::new(DataControlOffer {
            id: offer_id,
            client: client.clone(),
            source: Rc::downgrade(&(src.clone() as Rc<dyn SelectionSource>)),
        });
        let fd = rustix::event::eventfd(0, rustix::event::EventfdFlags::empty()).unwrap();
        use zwlr_data_control_offer_v1::Handler as _;
        offer
            .receive(zwlr_data_control_offer_v1::receive::Request {
                mime_type: "text/plain;charset=utf-8".to_string(),
                fd,
            })
            .unwrap();
        let bytes = client.queued_out_bytes();
        assert_eq!(count_events(&bytes, src.id, 0), 1, "source.send");
        // destroying the live source clears the selection
        src.destroy(zwlr_data_control_source_v1::destroy::Request {}).unwrap();
        assert!(seat_g.data.current_source().is_none());
    }
}

// -- the offer --

pub struct DataControlOffer {
    pub id: ObjectId,
    pub client: Rc<Client>,
    source: Weak<dyn SelectionSource>,
}

impl zwlr_data_control_offer_v1::Handler for DataControlOffer {
    fn receive(
        &self,
        req: zwlr_data_control_offer_v1::receive::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(src) = self.source.upgrade() {
            src.send(&req.mime_type, req.fd);
        }
        Ok(())
    }

    fn destroy(
        &self,
        _req: zwlr_data_control_offer_v1::destroy::Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.client.remove_obj(self.id)?;
        Ok(())
    }
}

impl Object for DataControlOffer {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn interface(&self) -> &'static str {
        zwlr_data_control_offer_v1::NAME
    }

    fn handle_request(
        self: Rc<Self>,
        opcode: u32,
        r: &mut MsgReader<'_>,
    ) -> Result<(), DispatchError> {
        zwlr_data_control_offer_v1::dispatch(&*self, 1, opcode, r)
    }
}
