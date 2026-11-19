use varlit::{varlit_format, StreamOptions, VarlitKeyedBlock};

varlit_format! {
    pub format HighCardinalityFormat {
        magic: b"HCIX";
        version: 1;
        blocks {
            fixed Account(id = 1, key = [id]) {
                id: u64,
                active: bool,
            }

            variable Frame(id = 2, key = [scan, frame]) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }
        }
    }
}

fn stream_reader_api(reader: &HighCardinalityFormatStreamReader) -> varlit::Result<()> {
    let _events = reader.events()?;
    let _accounts = reader.accounts()?;
    let _frames = reader.frames()?;
    let _state = reader.resident_state();
    Ok(())
}

fn stream_writer_api(writer: &mut HighCardinalityFormatStreamWriter) -> varlit::Result<()> {
    let account = Account {
        id: 1,
        active: true,
    };
    let frame = Frame {
        scan: 2,
        frame: 3,
        payload: vec![4],
    };
    writer.push_account(&account)?;
    writer.delete_account(&account.id)?;
    writer.push_frame(&frame)?;
    let frame_key = frame.key();
    writer.delete_frame(&frame_key)?;
    writer.flush()?;
    writer.sync()?;
    let _state = writer.resident_state();
    Ok(())
}

fn constructors(path: &std::path::Path) {
    let _ = HighCardinalityFormat::open_stream_reader(path, StreamOptions::default());
    let _ = HighCardinalityFormat::create_stream_writer(path, StreamOptions::default());
    let _ = HighCardinalityFormat::open_stream_writer(path, StreamOptions::default());
}

fn main() {
    fn assert_send<T: Send>() {}
    assert_send::<varlit::VarlitStreamReader>();
    assert_send::<HighCardinalityFormatStreamReader>();
    let _ = (
        stream_reader_api,
        stream_writer_api,
        constructors,
    );
}
