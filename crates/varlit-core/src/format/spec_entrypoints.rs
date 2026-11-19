// `FormatSpec` entry points: creating, opening, recovering and inspecting
// files of the format. Included by `format.rs`, whose module it shares.
impl FormatSpec {
    pub const fn builder() -> FormatSpecBuilder {
        FormatSpecBuilder::new()
    }

    pub fn create<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitFile::create(self_, path)
    }

    pub fn create_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitFile> {
        self.tighten_read_limits(limits).create(path)
    }

    pub fn create_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitFile> {
        self.with_resource_limits(limits).create(path)
    }

    pub fn create_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitFile::create(self_, path)
    }

    pub fn create_writer<P: AsRef<Path>>(self, path: P) -> Result<VarlitWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitWriter::create(self_, path)
    }

    pub fn create_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitWriter> {
        self.tighten_read_limits(limits).create_writer(path)
    }

    pub fn create_writer_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitWriter> {
        self.with_resource_limits(limits).create_writer(path)
    }

    pub fn create_writer_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarlitWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitWriter::create(self_, path)
    }

    pub fn create_with_dims<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarlitFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitFile::create_with_dims(self_, path, dims)
    }

    pub fn create_with_dims_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ReadLimits,
    ) -> Result<VarlitFile> {
        self.tighten_read_limits(limits)
            .create_with_dims(path, dims)
    }

    pub fn create_with_dims_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ResourceLimits,
    ) -> Result<VarlitFile> {
        self.with_resource_limits(limits)
            .create_with_dims(path, dims)
    }

    pub fn create_with_dims_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarlitFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitFile::create_with_dims(self_, path, dims)
    }

    pub fn create_writer_with_dims<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarlitWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitWriter::create_with_dims(self_, path, dims)
    }

    pub fn create_writer_with_dims_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ReadLimits,
    ) -> Result<VarlitWriter> {
        self.tighten_read_limits(limits)
            .create_writer_with_dims(path, dims)
    }

    pub fn create_writer_with_dims_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ResourceLimits,
    ) -> Result<VarlitWriter> {
        self.with_resource_limits(limits)
            .create_writer_with_dims(path, dims)
    }

    pub fn create_writer_with_dims_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarlitWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitWriter::create_with_dims(self_, path, dims)
    }

    pub fn open<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitFile::open(self_, path)
    }

    pub fn open_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitFile> {
        self.tighten_read_limits(limits).open(path)
    }

    pub fn open_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitFile> {
        self.with_resource_limits(limits).open(path)
    }

    pub fn open_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitFile::open(self_, path)
    }

    pub fn open_writer<P: AsRef<Path>>(self, path: P) -> Result<VarlitWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitWriter::open(self_, path)
    }

    pub fn open_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitWriter> {
        self.tighten_read_limits(limits).open_writer(path)
    }

    pub fn open_writer_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitWriter> {
        self.with_resource_limits(limits).open_writer(path)
    }

    pub fn open_writer_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarlitWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitWriter::open(self_, path)
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        self,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<VarlitFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitFile::open_with_lock_policy(self_, path, policy)
    }

    pub fn open_writer_with_lock_policy<P: AsRef<Path>>(
        self,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<VarlitWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitWriter::open_with_lock_policy(self_, path, policy)
    }

    /// Clears a stale writer lock without opening or scanning the data file.
    pub fn clear_stale_writer_lock<P: AsRef<Path>>(
        self,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<()> {
        self.validate()?;
        crate::clear_stale_writer_lock(path, policy)
    }

    pub fn open_readonly<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitFile::open_readonly(self_, path)
    }

    pub fn open_readonly_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitFile> {
        self.tighten_read_limits(limits).open_readonly(path)
    }

    pub fn open_readonly_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitFile> {
        self.with_resource_limits(limits).open_readonly(path)
    }

    pub fn open_readonly_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitFile::open_readonly(self_, path)
    }

    pub fn open_reader<P: AsRef<Path>>(self, path: P) -> Result<VarlitReader> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitReader::open(self_, path)
    }

    pub fn open_reader_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitReader> {
        self.tighten_read_limits(limits).open_reader(path)
    }

    pub fn open_reader_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitReader> {
        self.with_resource_limits(limits).open_reader(path)
    }

    pub fn open_reader_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarlitReader> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitReader::open(self_, path)
    }

    pub fn open_recover<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitFile::open_recover(self_, path)
    }

    pub fn open_recover_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitFile> {
        self.tighten_read_limits(limits).open_recover(path)
    }

    pub fn open_recover_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitFile> {
        self.with_resource_limits(limits).open_recover(path)
    }

    pub fn open_recover_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarlitFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitFile::open_recover(self_, path)
    }

    pub fn open_recover_writer<P: AsRef<Path>>(self, path: P) -> Result<VarlitWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitWriter::open_recover(self_, path)
    }

    pub fn open_recover_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarlitWriter> {
        self.tighten_read_limits(limits).open_recover_writer(path)
    }

    pub fn open_recover_writer_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarlitWriter> {
        self.with_resource_limits(limits).open_recover_writer(path)
    }

    pub fn open_recover_writer_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<VarlitWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitWriter::open_recover(self_, path)
    }

    pub fn open_recover_with_report<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarlitFile, crate::RecoveryReport)> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitFile::open_recover_with_report(self_, path)
    }

    pub fn open_recover_with_report_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<(VarlitFile, crate::RecoveryReport)> {
        self.tighten_read_limits(limits)
            .open_recover_with_report(path)
    }

    pub fn open_recover_with_report_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<(VarlitFile, crate::RecoveryReport)> {
        self.with_resource_limits(limits)
            .open_recover_with_report(path)
    }

    pub fn open_recover_with_report_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarlitFile, crate::RecoveryReport)> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitFile::open_recover_with_report(self_, path)
    }

    pub fn open_recover_writer_with_report<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarlitWriter, crate::RecoveryReport)> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarlitWriter::open_recover_with_report(self_, path)
    }

    pub fn open_recover_writer_with_report_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<(VarlitWriter, crate::RecoveryReport)> {
        self.tighten_read_limits(limits)
            .open_recover_writer_with_report(path)
    }

    pub fn open_recover_writer_with_report_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<(VarlitWriter, crate::RecoveryReport)> {
        self.with_resource_limits(limits)
            .open_recover_writer_with_report(path)
    }

    pub fn open_recover_writer_with_report_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarlitWriter, crate::RecoveryReport)> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarlitWriter::open_recover_with_report(self_, path)
    }

    pub fn inspect_writer_lock<P: AsRef<Path>>(self, path: P) -> Result<Option<WriterLockInfo>> {
        self.validate()?;
        VarlitFile::inspect_writer_lock(path)
    }

    pub fn diagnostics(self) -> crate::FormatDiagnostics {
        crate::diagnose_spec(self)
    }

    pub fn diagnose_file<P: AsRef<Path>>(self, path: P) -> crate::FormatDiagnostics {
        crate::diagnose_file(self, path)
    }

    pub fn self_test<P: AsRef<Path>>(self, path: P) -> crate::FormatSelfTest {
        crate::FormatSelfTest::new(self, path)
    }

    pub fn block(self, id: u32) -> Option<BlockDescriptor> {
        self.blocks.iter().copied().find(|block| block.id == id)
    }

    pub fn effective_layout(&self) -> LayoutPlan {
        if self.layout.is_varlit_native_default() {
            native_layout_plan(*self)
        } else {
            layout_spec_to_plan(self.layout)
        }
    }
}
