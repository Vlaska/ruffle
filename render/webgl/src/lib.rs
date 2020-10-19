use ruffle_core::backend::render::swf;
use ruffle_core::backend::render::{
    srgb_to_linear, Bitmap, BitmapFormat, BitmapHandle, BitmapInfo, Color, Letterbox,
    RenderBackend, ShapeHandle, Transform,
};
use ruffle_core::shape_utils::DistilledShape;
use ruffle_core::swf::Matrix;
use ruffle_render_common_tess::{GradientSpread, GradientType, ShapeTessellator, Vertex};
use ruffle_web_common::JsResult;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    HtmlCanvasElement, OesVertexArrayObject, WebGl2RenderingContext as Gl2, WebGlBuffer,
    WebGlFramebuffer, WebGlProgram, WebGlRenderbuffer, WebGlRenderingContext as Gl, WebGlShader,
    WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject, WebglDebugRendererInfo,
};

type Error = Box<dyn std::error::Error>;

const COLOR_VERTEX_GLSL: &str = include_str!("../shaders/color.vert");
const COLOR_FRAGMENT_GLSL: &str = include_str!("../shaders/color.frag");
const TEXTURE_VERTEX_GLSL: &str = include_str!("../shaders/texture.vert");
const GRADIENT_FRAGMENT_GLSL: &str = include_str!("../shaders/gradient.frag");
const BITMAP_FRAGMENT_GLSL: &str = include_str!("../shaders/bitmap.frag");
const NUM_VERTEX_ATTRIBUTES: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskState {
    NoMask,
    DrawMaskStencil,
    DrawMaskedContent,
    ClearMaskStencil,
}

pub struct WebGlRenderBackend {
    /// WebGL1 context
    gl: Gl,

    // WebGL2 context, if available.
    gl2: Option<Gl2>,

    /// In WebGL1, VAOs are only available as an extension.
    vao_ext: OesVertexArrayObject,

    // The frame buffers used for resolving MSAA.
    msaa_buffers: Option<MsaaBuffers>,
    msaa_sample_count: u32,

    color_program: ShaderProgram,
    bitmap_program: ShaderProgram,
    gradient_program: ShaderProgram,

    shape_tessellator: ShapeTessellator,

    textures: Vec<(swf::CharacterId, Texture)>,
    meshes: Vec<Mesh>,

    quad_shape: ShapeHandle,

    mask_state: MaskState,
    num_masks: u32,
    mask_state_dirty: bool,

    active_program: *const ShaderProgram,
    blend_func: (u32, u32),
    mult_color: Option<[f32; 4]>,
    add_color: Option<[f32; 4]>,

    viewport_width: f32,
    viewport_height: f32,
    view_matrix: [[f32; 4]; 4],
}

const MAX_GRADIENT_COLORS: usize = 15;

impl WebGlRenderBackend {
    pub fn new(canvas: &HtmlCanvasElement) -> Result<Self, Error> {
        // Create WebGL context.
        let options = [
            ("stencil", JsValue::TRUE),
            ("alpha", JsValue::FALSE),
            ("antialias", JsValue::FALSE),
            ("depth", JsValue::FALSE),
            ("failIfMajorPerformanceCaveat", JsValue::TRUE), // fail if no GPU available
        ];
        let context_options = js_sys::Object::new();
        for (name, value) in options.iter() {
            js_sys::Reflect::set(&context_options, &JsValue::from(*name), value).warn_on_error();
        }

        // Attempt to create a WebGL2 context, but fall back to WebGL1 if unavailable.
        let (gl, gl2, vao_ext, msaa_sample_count) = if let Ok(Some(gl)) =
            canvas.get_context_with_context_options("webgl2", &context_options)
        {
            log::info!("Creating WebGL2 context.");
            let gl2 = gl.dyn_into::<Gl2>().map_err(|_| "Expected GL context")?;

            // Determine MSAA sample count.
            // Default to 4x MSAA on desktop, 2x on mobile/tablets.
            let mut msaa_sample_count = if ruffle_web_common::is_mobile_or_tablet() {
                log::info!("Running on a mobile device; defaulting to 2x MSAA");
                2
            } else {
                4
            };

            // Ensure that we don't exceed the max MSAA of this device.
            if let Ok(max_samples) = gl2.get_parameter(Gl2::MAX_SAMPLES) {
                let max_samples: u32 = max_samples.as_f64().unwrap_or(0.0) as u32;
                if max_samples > 0 && max_samples < msaa_sample_count {
                    log::info!("Device only supports {}xMSAA", max_samples);
                    msaa_sample_count = max_samples;
                }
            }

            // WebGLRenderingContext inherits from WebGL2RenderingContext, so cast it down.
            (
                gl2.clone().unchecked_into::<Gl>(),
                Some(gl2),
                JsValue::UNDEFINED.unchecked_into(),
                msaa_sample_count,
            )
        } else {
            // Fall back to WebGL1.
            // Request antialiasing on WebGL1, because there isn't general MSAA support.
            js_sys::Reflect::set(
                &context_options,
                &JsValue::from("antialias"),
                &JsValue::TRUE,
            )
            .warn_on_error();

            if let Ok(Some(gl)) = canvas.get_context_with_context_options("webgl", &context_options)
            {
                log::info!("Falling back to WebGL1.");

                let gl = gl.dyn_into::<Gl>().map_err(|_| "Expected GL context")?;
                // `dyn_into` doesn't work here; why?
                let vao = gl
                    .get_extension("OES_vertex_array_object")
                    .into_js_result()?
                    .ok_or("VAO extension not found")?
                    .unchecked_into::<OesVertexArrayObject>();
                (gl, None, vao, 1)
            } else {
                return Err("Unable to create WebGL rendering context".into());
            }
        };

        // Get WebGL driver info.
        let driver_info = if gl.get_extension("WEBGL_debug_renderer_info").is_ok() {
            gl.get_parameter(WebglDebugRendererInfo::UNMASKED_RENDERER_WEBGL)
                .ok()
                .and_then(|val| val.as_string())
                .unwrap_or_else(|| "<unknown>".to_string())
        } else {
            "<unknown>".to_string()
        };

        log::info!("WebGL graphics driver: {}", driver_info);

        let color_vertex = Self::compile_shader(&gl, Gl::VERTEX_SHADER, COLOR_VERTEX_GLSL)?;
        let texture_vertex = Self::compile_shader(&gl, Gl::VERTEX_SHADER, TEXTURE_VERTEX_GLSL)?;
        let color_fragment = Self::compile_shader(&gl, Gl::FRAGMENT_SHADER, COLOR_FRAGMENT_GLSL)?;
        let bitmap_fragment = Self::compile_shader(&gl, Gl::FRAGMENT_SHADER, BITMAP_FRAGMENT_GLSL)?;
        let gradient_fragment =
            Self::compile_shader(&gl, Gl::FRAGMENT_SHADER, GRADIENT_FRAGMENT_GLSL)?;

        let color_program = ShaderProgram::new(&gl, &color_vertex, &color_fragment)?;
        let bitmap_program = ShaderProgram::new(&gl, &texture_vertex, &bitmap_fragment)?;
        let gradient_program = ShaderProgram::new(&gl, &texture_vertex, &gradient_fragment)?;

        gl.enable(Gl::BLEND);
        gl.blend_func(Gl::SRC_ALPHA, Gl::ONE_MINUS_SRC_ALPHA);

        // Necessary to load RGB textures (alignment defaults to 4).
        gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 1);

        let mut renderer = Self {
            gl,
            gl2,
            vao_ext,

            msaa_buffers: None,
            msaa_sample_count,

            color_program,
            gradient_program,
            bitmap_program,

            shape_tessellator: ShapeTessellator::new(),

            meshes: vec![],
            quad_shape: ShapeHandle(0),
            textures: vec![],
            viewport_width: 500.0,
            viewport_height: 500.0,
            view_matrix: [[0.0; 4]; 4],

            mask_state: MaskState::NoMask,
            num_masks: 0,
            mask_state_dirty: true,

            active_program: std::ptr::null(),
            blend_func: (Gl::SRC_ALPHA, Gl::ONE_MINUS_SRC_ALPHA),
            mult_color: None,
            add_color: None,
        };

        let quad_mesh = renderer.build_quad_mesh()?;
        renderer.meshes.push(quad_mesh);
        renderer.build_msaa_buffers()?;
        renderer.build_matrices();

        Ok(renderer)
    }

    fn build_quad_mesh(&mut self) -> Result<Mesh, Error> {
        let vao = self.create_vertex_array()?;

        let vertex_buffer = self.gl.create_buffer().unwrap();
        self.gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&vertex_buffer));

        let verts = [
            Vertex {
                position: [0.0, 0.0],
                color: 0xffff_ffff,
            },
            Vertex {
                position: [1.0, 0.0],
                color: 0xffff_ffff,
            },
            Vertex {
                position: [1.0, 1.0],
                color: 0xffff_ffff,
            },
            Vertex {
                position: [0.0, 1.0],
                color: 0xffff_ffff,
            },
        ];
        let (vertex_buffer, index_buffer) = unsafe {
            let verts_bytes = std::slice::from_raw_parts(
                verts.as_ptr() as *const u8,
                std::mem::size_of_val(&verts),
            );
            self.gl
                .buffer_data_with_u8_array(Gl::ARRAY_BUFFER, verts_bytes, Gl::STATIC_DRAW);

            let index_buffer = self.gl.create_buffer().unwrap();
            self.gl
                .bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&index_buffer));
            let indices = [0u16, 1, 2, 0, 2, 3];
            let indices_bytes = std::slice::from_raw_parts(
                indices.as_ptr() as *const u8,
                std::mem::size_of::<u16>() * indices.len(),
            );
            self.gl.buffer_data_with_u8_array(
                Gl::ELEMENT_ARRAY_BUFFER,
                indices_bytes,
                Gl::STATIC_DRAW,
            );

            (vertex_buffer, index_buffer)
        };

        let program = &self.bitmap_program;
        if program.vertex_position_location != 0xffff_ffff {
            self.gl.vertex_attrib_pointer_with_i32(
                program.vertex_position_location,
                2,
                Gl::FLOAT,
                false,
                12,
                0,
            );
            self.gl
                .enable_vertex_attrib_array(program.vertex_position_location as u32);
        }

        if program.vertex_color_location != 0xffff_ffff {
            self.gl.vertex_attrib_pointer_with_i32(
                program.vertex_color_location,
                4,
                Gl::UNSIGNED_BYTE,
                true,
                12,
                8,
            );
            self.gl
                .enable_vertex_attrib_array(program.vertex_color_location as u32);
        }
        for i in program.num_vertex_attributes..NUM_VERTEX_ATTRIBUTES {
            self.gl.disable_vertex_attrib_array(i);
        }

        let quad_mesh = Mesh {
            draws: vec![Draw {
                draw_type: DrawType::Bitmap(BitmapDraw {
                    matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                    id: 0,

                    is_smoothed: true,
                    is_repeating: false,
                }),
                vao,
                vertex_buffer,
                index_buffer,
                num_indices: 6,
            }],
        };
        Ok(quad_mesh)
    }

    fn compile_shader(gl: &Gl, shader_type: u32, glsl_src: &str) -> Result<WebGlShader, Error> {
        let shader = gl.create_shader(shader_type).unwrap();
        gl.shader_source(&shader, glsl_src);
        gl.compile_shader(&shader);
        let log = gl.get_shader_info_log(&shader).unwrap_or_default();
        if !log.is_empty() {
            log::error!("{}", log);
        }
        Ok(shader)
    }

    fn build_msaa_buffers(&mut self) -> Result<(), Error> {
        if self.gl2.is_none() || self.msaa_sample_count <= 1 {
            self.gl.bind_framebuffer(Gl::FRAMEBUFFER, None);
            self.gl.bind_renderbuffer(Gl::RENDERBUFFER, None);
            return Ok(());
        }

        let gl = self.gl2.as_ref().unwrap();

        // Delete previous buffers, if they exist.
        if let Some(msaa_buffers) = self.msaa_buffers.take() {
            gl.delete_renderbuffer(Some(&msaa_buffers.color_renderbuffer));
            gl.delete_renderbuffer(Some(&msaa_buffers.stencil_renderbuffer));
            gl.delete_framebuffer(Some(&msaa_buffers.render_framebuffer));
            gl.delete_framebuffer(Some(&msaa_buffers.color_framebuffer));
            gl.delete_texture(Some(&msaa_buffers.framebuffer_texture));
        }

        // Create frame and render buffers.
        let render_framebuffer = gl
            .create_framebuffer()
            .ok_or("Unable to create framebuffer")?;
        let color_framebuffer = gl
            .create_framebuffer()
            .ok_or("Unable to create framebuffer")?;

        // Note for future self:
        // Whenever we support playing transparent movies,
        // switch this to RGBA and probably need to change shaders to all
        // be premultiplied alpha.
        let color_renderbuffer = gl
            .create_renderbuffer()
            .ok_or("Unable to create renderbuffer")?;
        gl.bind_renderbuffer(Gl2::RENDERBUFFER, Some(&color_renderbuffer));
        gl.renderbuffer_storage_multisample(
            Gl2::RENDERBUFFER,
            self.msaa_sample_count as i32,
            Gl2::RGB8,
            self.viewport_width as i32,
            self.viewport_height as i32,
        );
        gl.check_error("renderbuffer_storage_multisample (color)")?;

        let stencil_renderbuffer = gl
            .create_renderbuffer()
            .ok_or("Unable to create renderbuffer")?;
        gl.bind_renderbuffer(Gl2::RENDERBUFFER, Some(&stencil_renderbuffer));
        gl.renderbuffer_storage_multisample(
            Gl2::RENDERBUFFER,
            self.msaa_sample_count as i32,
            Gl2::STENCIL_INDEX8,
            self.viewport_width as i32,
            self.viewport_height as i32,
        );
        gl.check_error("renderbuffer_storage_multisample (stencil)")?;

        gl.bind_framebuffer(Gl2::FRAMEBUFFER, Some(&render_framebuffer));
        gl.framebuffer_renderbuffer(
            Gl2::FRAMEBUFFER,
            Gl2::COLOR_ATTACHMENT0,
            Gl2::RENDERBUFFER,
            Some(&color_renderbuffer),
        );
        gl.framebuffer_renderbuffer(
            Gl2::FRAMEBUFFER,
            Gl2::STENCIL_ATTACHMENT,
            Gl2::RENDERBUFFER,
            Some(&stencil_renderbuffer),
        );

        let framebuffer_texture = gl.create_texture().ok_or("Unable to create texture")?;
        gl.bind_texture(Gl2::TEXTURE_2D, Some(&framebuffer_texture));
        gl.tex_parameteri(Gl2::TEXTURE_2D, Gl2::TEXTURE_MAG_FILTER, Gl::NEAREST as i32);
        gl.tex_parameteri(Gl2::TEXTURE_2D, Gl2::TEXTURE_MIN_FILTER, Gl::NEAREST as i32);
        gl.tex_parameteri(
            Gl2::TEXTURE_2D,
            Gl2::TEXTURE_WRAP_S,
            Gl::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameteri(
            Gl2::TEXTURE_2D,
            Gl2::TEXTURE_WRAP_T,
            Gl2::CLAMP_TO_EDGE as i32,
        );
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl2::TEXTURE_2D,
            0,
            Gl2::RGB as i32,
            self.viewport_width as i32,
            self.viewport_height as i32,
            0,
            Gl2::RGB,
            Gl2::UNSIGNED_BYTE,
            None,
        )
        .into_js_result()?;
        gl.bind_texture(Gl2::TEXTURE_2D, None);

        gl.bind_framebuffer(Gl2::FRAMEBUFFER, Some(&color_framebuffer));
        gl.framebuffer_texture_2d(
            Gl2::FRAMEBUFFER,
            Gl2::COLOR_ATTACHMENT0,
            Gl2::TEXTURE_2D,
            Some(&framebuffer_texture),
            0,
        );
        gl.bind_framebuffer(Gl2::FRAMEBUFFER, None);

        self.msaa_buffers = Some(MsaaBuffers {
            render_framebuffer,
            color_renderbuffer,
            stencil_renderbuffer,
            color_framebuffer,
            framebuffer_texture,
        });

        Ok(())
    }

    fn register_shape_internal(&mut self, shape: DistilledShape) -> Mesh {
        use ruffle_render_common_tess::DrawType as TessDrawType;

        let textures = &self.textures;
        let lyon_mesh = self.shape_tessellator.tessellate_shape(shape, |id| {
            textures
                .iter()
                .find(|(other_id, _tex)| *other_id == id)
                .map(|tex| (tex.1.width, tex.1.height))
        });

        let mut draws = Vec::with_capacity(lyon_mesh.len());

        for draw in lyon_mesh {
            let num_indices = draw.indices.len() as i32;

            let vao = self.create_vertex_array().unwrap();
            let vertex_buffer = self.gl.create_buffer().unwrap();
            self.gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&vertex_buffer));

            let (vertex_buffer, index_buffer) = unsafe {
                let verts_bytes = std::slice::from_raw_parts(
                    draw.vertices.as_ptr() as *const u8,
                    draw.vertices.len() * std::mem::size_of::<Vertex>(),
                );
                self.gl
                    .buffer_data_with_u8_array(Gl::ARRAY_BUFFER, verts_bytes, Gl::STATIC_DRAW);

                let indices: Vec<_> = draw.indices.into_iter().map(|n| n as u16).collect();
                let index_buffer = self.gl.create_buffer().unwrap();
                self.gl
                    .bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&index_buffer));
                let indices_bytes = std::slice::from_raw_parts(
                    indices.as_ptr() as *const u8,
                    indices.len() * std::mem::size_of::<u16>(),
                );
                self.gl.buffer_data_with_u8_array(
                    Gl::ELEMENT_ARRAY_BUFFER,
                    indices_bytes,
                    Gl::STATIC_DRAW,
                );
                (vertex_buffer, index_buffer)
            };

            let (program, out_draw) = match draw.draw_type {
                TessDrawType::Color => (
                    &self.color_program,
                    Draw {
                        draw_type: DrawType::Color,
                        vao,
                        vertex_buffer,
                        index_buffer,
                        num_indices,
                    },
                ),
                TessDrawType::Gradient(gradient) => {
                    let mut ratios = [0.0; MAX_GRADIENT_COLORS];
                    let mut colors = [[0.0; 4]; MAX_GRADIENT_COLORS];
                    let num_colors = (gradient.num_colors as usize).min(MAX_GRADIENT_COLORS);
                    ratios[..num_colors].copy_from_slice(&gradient.ratios[..num_colors]);
                    colors[..num_colors].copy_from_slice(&gradient.colors[..num_colors]);
                    // Convert to linear color space if this is a linear-interpolated gradient.
                    if gradient.interpolation == swf::GradientInterpolation::LinearRGB {
                        for color in &mut colors[..num_colors] {
                            *color = srgb_to_linear(*color);
                        }
                    }
                    for i in num_colors..MAX_GRADIENT_COLORS {
                        ratios[i] = ratios[i - 1];
                        colors[i] = colors[i - 1];
                    }
                    let out_gradient = Gradient {
                        matrix: gradient.matrix,
                        gradient_type: match gradient.gradient_type {
                            GradientType::Linear => 0,
                            GradientType::Radial => 1,
                            GradientType::Focal => 2,
                        },
                        ratios,
                        colors,
                        num_colors: gradient.num_colors,
                        repeat_mode: match gradient.repeat_mode {
                            GradientSpread::Pad => 0,
                            GradientSpread::Repeat => 1,
                            GradientSpread::Reflect => 2,
                        },
                        focal_point: gradient.focal_point,
                        interpolation: gradient.interpolation,
                    };
                    (
                        &self.gradient_program,
                        Draw {
                            draw_type: DrawType::Gradient(Box::new(out_gradient)),
                            vao,
                            vertex_buffer,
                            index_buffer,
                            num_indices,
                        },
                    )
                }
                TessDrawType::Bitmap(bitmap) => (
                    &self.bitmap_program,
                    Draw {
                        draw_type: DrawType::Bitmap(BitmapDraw {
                            matrix: bitmap.matrix,
                            id: bitmap.id,
                            is_smoothed: bitmap.is_smoothed,
                            is_repeating: bitmap.is_repeating,
                        }),
                        vao,
                        vertex_buffer,
                        index_buffer,
                        num_indices,
                    },
                ),
            };

            // Unfortunately it doesn't seem to be possible to ensure that vertex attributes will be in
            // a guaranteed position between shaders in WebGL1 (no layout qualifiers in GLSL in OpenGL ES 1.0).
            // Attributes can change between shaders, even if the vertex layout is otherwise "the same".
            // This varies between platforms based on what the GLSL compiler decides to do.
            if program.vertex_position_location != 0xffff_ffff {
                self.gl.vertex_attrib_pointer_with_i32(
                    program.vertex_position_location,
                    2,
                    Gl::FLOAT,
                    false,
                    12,
                    0,
                );
                self.gl
                    .enable_vertex_attrib_array(program.vertex_position_location as u32);
            }

            if program.vertex_color_location != 0xffff_ffff {
                self.gl.vertex_attrib_pointer_with_i32(
                    program.vertex_color_location,
                    4,
                    Gl::UNSIGNED_BYTE,
                    true,
                    12,
                    8,
                );
                self.gl
                    .enable_vertex_attrib_array(program.vertex_color_location as u32);
            }

            draws.push(out_draw);
            self.bind_vertex_array(None);

            for i in program.num_vertex_attributes..NUM_VERTEX_ATTRIBUTES {
                self.gl.disable_vertex_attrib_array(i);
            }
        }

        Mesh { draws }
    }

    fn build_matrices(&mut self) {
        self.view_matrix = [
            [1.0 / (self.viewport_width as f32 / 2.0), 0.0, 0.0, 0.0],
            [0.0, -1.0 / (self.viewport_height as f32 / 2.0), 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0, 1.0],
        ];
    }

    /// Creates and binds a new VAO.
    fn create_vertex_array(&self) -> Result<WebGlVertexArrayObject, Error> {
        let vao = if let Some(gl2) = &self.gl2 {
            let vao = gl2.create_vertex_array().ok_or("Unable to create VAO")?;
            gl2.bind_vertex_array(Some(&vao));
            vao
        } else {
            let vao = self
                .vao_ext
                .create_vertex_array_oes()
                .ok_or("Unable to create VAO")?;
            self.vao_ext.bind_vertex_array_oes(Some(&vao));
            vao
        };
        Ok(vao)
    }

    /// Binds a VAO.
    fn bind_vertex_array(&self, vao: Option<&WebGlVertexArrayObject>) {
        if let Some(gl2) = &self.gl2 {
            gl2.bind_vertex_array(vao);
        } else {
            self.vao_ext.bind_vertex_array_oes(vao);
        };
    }

    fn set_stencil_state(&mut self) {
        // Set stencil state for masking, if necessary.
        if self.mask_state_dirty {
            match self.mask_state {
                MaskState::NoMask => {
                    self.gl.disable(Gl::STENCIL_TEST);
                    self.gl.color_mask(true, true, true, true);
                }
                MaskState::DrawMaskStencil => {
                    self.gl.enable(Gl::STENCIL_TEST);
                    self.gl
                        .stencil_func(Gl::EQUAL, (self.num_masks - 1) as i32, 0xff);
                    self.gl.stencil_op(Gl::KEEP, Gl::KEEP, Gl::INCR);
                    self.gl.color_mask(false, false, false, false);
                }
                MaskState::DrawMaskedContent => {
                    self.gl.enable(Gl::STENCIL_TEST);
                    self.gl.stencil_func(Gl::EQUAL, self.num_masks as i32, 0xff);
                    self.gl.stencil_op(Gl::KEEP, Gl::KEEP, Gl::KEEP);
                    self.gl.color_mask(true, true, true, true);
                }
                MaskState::ClearMaskStencil => {
                    self.gl.enable(Gl::STENCIL_TEST);
                    self.gl.stencil_func(Gl::EQUAL, self.num_masks as i32, 0xff);
                    self.gl.stencil_op(Gl::KEEP, Gl::KEEP, Gl::DECR);
                    self.gl.color_mask(false, false, false, false);
                }
            }
        }
    }

    fn register_bitmap(
        &mut self,
        id: swf::CharacterId,
        bitmap: Bitmap,
    ) -> Result<BitmapInfo, Error> {
        let texture = self.gl.create_texture().unwrap();
        self.gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
        match bitmap.data {
            BitmapFormat::Rgb(data) => self
                .gl
                .tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
                    Gl::TEXTURE_2D,
                    0,
                    Gl::RGB as i32,
                    bitmap.width as i32,
                    bitmap.height as i32,
                    0,
                    Gl::RGB,
                    Gl::UNSIGNED_BYTE,
                    Some(&data),
                )
                .into_js_result()?,
            BitmapFormat::Rgba(data) => self
                .gl
                .tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
                    Gl::TEXTURE_2D,
                    0,
                    Gl::RGBA as i32,
                    bitmap.width as i32,
                    bitmap.height as i32,
                    0,
                    Gl::RGBA,
                    Gl::UNSIGNED_BYTE,
                    Some(&data),
                )
                .into_js_result()?,
        }

        // You must set the texture parameters for non-power-of-2 textures to function in WebGL1.
        self.gl
            .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
        self.gl
            .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);
        self.gl
            .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::LINEAR as i32);
        self.gl
            .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::LINEAR as i32);

        let handle = BitmapHandle(self.textures.len());
        self.textures.push((
            id,
            Texture {
                texture,
                width: bitmap.width,
                height: bitmap.height,
            },
        ));

        Ok(BitmapInfo {
            handle,
            width: bitmap.width as u16,
            height: bitmap.height as u16,
        })
    }
}

impl RenderBackend for WebGlRenderBackend {
    fn set_viewport_dimensions(&mut self, width: u32, height: u32) {
        self.viewport_width = width as f32;
        self.viewport_height = height as f32;
        self.gl.viewport(0, 0, width as i32, height as i32);
        self.build_msaa_buffers().unwrap();
        self.build_matrices();
    }

    fn register_shape(&mut self, shape: DistilledShape) -> ShapeHandle {
        let handle = ShapeHandle(self.meshes.len());
        let mesh = self.register_shape_internal(shape);
        self.meshes.push(mesh);
        handle
    }

    fn replace_shape(&mut self, shape: DistilledShape, handle: ShapeHandle) {
        let mesh = self.register_shape_internal(shape);
        self.meshes[handle.0] = mesh;
    }

    fn register_glyph_shape(&mut self, glyph: &swf::Glyph) -> ShapeHandle {
        let shape = ruffle_core::shape_utils::swf_glyph_to_shape(glyph);
        let handle = ShapeHandle(self.meshes.len());
        let mesh = self.register_shape_internal((&shape).into());
        self.meshes.push(mesh);
        handle
    }

    fn register_bitmap_jpeg(
        &mut self,
        id: swf::CharacterId,
        data: &[u8],
        jpeg_tables: Option<&[u8]>,
    ) -> Result<BitmapInfo, Error> {
        let data = ruffle_core::backend::render::glue_tables_to_jpeg(data, jpeg_tables);
        self.register_bitmap_jpeg_2(id, &data[..])
    }

    fn register_bitmap_jpeg_2(
        &mut self,
        id: swf::CharacterId,
        data: &[u8],
    ) -> Result<BitmapInfo, Error> {
        let bitmap = ruffle_core::backend::render::decode_define_bits_jpeg(data, None)?;
        self.register_bitmap(id, bitmap)
    }

    fn register_bitmap_jpeg_3(
        &mut self,
        id: swf::CharacterId,
        jpeg_data: &[u8],
        alpha_data: &[u8],
    ) -> Result<BitmapInfo, Error> {
        let bitmap =
            ruffle_core::backend::render::decode_define_bits_jpeg(jpeg_data, Some(alpha_data))?;
        self.register_bitmap(id, bitmap)
    }

    fn register_bitmap_png(
        &mut self,
        swf_tag: &swf::DefineBitsLossless,
    ) -> Result<BitmapInfo, Error> {
        let bitmap = ruffle_core::backend::render::decode_define_bits_lossless(swf_tag)?;
        self.register_bitmap(swf_tag.id, bitmap)
    }

    fn begin_frame(&mut self, clear: Color) {
        self.active_program = std::ptr::null();
        self.mask_state = MaskState::NoMask;
        self.num_masks = 0;
        self.mask_state_dirty = true;

        self.mult_color = None;
        self.add_color = None;

        // Bind to MSAA render buffer if using MSAA.
        if let Some(msaa_buffers) = &self.msaa_buffers {
            let gl = &self.gl;
            gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&msaa_buffers.render_framebuffer));
        }

        self.set_stencil_state();
        self.gl.clear_color(
            clear.r as f32 / 255.0,
            clear.g as f32 / 255.0,
            clear.b as f32 / 255.0,
            clear.a as f32 / 255.0,
        );
        self.gl.stencil_mask(0xff);
        self.gl.clear(Gl::COLOR_BUFFER_BIT | Gl::STENCIL_BUFFER_BIT);
    }

    fn end_frame(&mut self) {
        // Resolve MSAA, if we're using it (WebGL2).
        if let (Some(ref gl), Some(ref msaa_buffers)) = (&self.gl2, &self.msaa_buffers) {
            self.gl.disable(Gl::STENCIL_TEST);

            // Resolve the MSAA in the render buffer.
            gl.bind_framebuffer(
                Gl2::READ_FRAMEBUFFER,
                Some(&msaa_buffers.render_framebuffer),
            );
            gl.bind_framebuffer(Gl2::DRAW_FRAMEBUFFER, Some(&msaa_buffers.color_framebuffer));
            gl.blit_framebuffer(
                0,
                0,
                self.viewport_width as i32,
                self.viewport_height as i32,
                0,
                0,
                self.viewport_width as i32,
                self.viewport_height as i32,
                Gl2::COLOR_BUFFER_BIT,
                Gl2::NEAREST,
            );

            // Render the resolved framebuffer texture to a quad on the screen.
            gl.bind_framebuffer(Gl2::FRAMEBUFFER, None);
            let program = &self.bitmap_program;
            self.gl.use_program(Some(&program.program));

            // Scale to fill screen.
            program.uniform_matrix4fv(
                &self.gl,
                ShaderUniform::WorldMatrix,
                &[
                    [2.0, 0.0, 0.0, 0.0],
                    [0.0, 2.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [-1.0, -1.0, 0.0, 1.0],
                ],
            );
            program.uniform_matrix4fv(
                &self.gl,
                ShaderUniform::ViewMatrix,
                &[
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [0.0, 0.0, 0.0, 1.0],
                ],
            );
            program.uniform4fv(&self.gl, ShaderUniform::MultColor, &[1.0, 1.0, 1.0, 1.0]);
            program.uniform4fv(&self.gl, ShaderUniform::AddColor, &[0.0, 0.0, 0.0, 0.0]);

            program.uniform_matrix3fv(
                &self.gl,
                ShaderUniform::TextureMatrix,
                &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            );

            // Bind the framebuffer texture.
            self.gl.active_texture(Gl2::TEXTURE0);
            self.gl
                .bind_texture(Gl2::TEXTURE_2D, Some(&msaa_buffers.framebuffer_texture));
            program.uniform1i(&self.gl, ShaderUniform::BitmapTexture, 0);

            // Render the quad.
            let quad = &self.meshes[self.quad_shape.0];
            self.bind_vertex_array(Some(&quad.draws[0].vao));
            self.gl.draw_elements_with_i32(
                Gl::TRIANGLES,
                quad.draws[0].num_indices,
                Gl::UNSIGNED_SHORT,
                0,
            );
        }
    }

    fn render_bitmap(&mut self, bitmap: BitmapHandle, transform: &Transform) {
        // TODO: Might be better to make this separate code to render the bitmap
        // instead of going through render_shape. But render_shape already handles
        // masking etc.
        if let Some((id, bitmap)) = self.textures.get(bitmap.0) {
            // Adjust the quad draw to use the target bitmap.
            let mesh = &mut self.meshes[self.quad_shape.0];
            let draw = &mut mesh.draws[0];
            let width = bitmap.width as f32;
            let height = bitmap.height as f32;
            if let DrawType::Bitmap(BitmapDraw { id: draw_id, .. }) = &mut draw.draw_type {
                *draw_id = *id;
            }

            // Scale the quad to the bitmap's dimensions.
            let scale_transform = Transform {
                matrix: transform.matrix
                    * Matrix {
                        a: width,
                        d: height,
                        ..Default::default()
                    },
                ..*transform
            };

            // Render the quad.
            self.render_shape(self.quad_shape, &scale_transform);
        }
    }

    fn render_shape(&mut self, shape: ShapeHandle, transform: &Transform) {
        let world_matrix = [
            [transform.matrix.a, transform.matrix.b, 0.0, 0.0],
            [transform.matrix.c, transform.matrix.d, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [
                transform.matrix.tx.to_pixels() as f32,
                transform.matrix.ty.to_pixels() as f32,
                0.0,
                1.0,
            ],
        ];

        let mult_color = [
            transform.color_transform.r_mult,
            transform.color_transform.g_mult,
            transform.color_transform.b_mult,
            transform.color_transform.a_mult,
        ];

        let add_color = [
            transform.color_transform.r_add,
            transform.color_transform.g_add,
            transform.color_transform.b_add,
            transform.color_transform.a_add,
        ];

        self.set_stencil_state();

        let mesh = &self.meshes[shape.0];
        for draw in &mesh.draws {
            self.bind_vertex_array(Some(&draw.vao));

            let (program, src_blend, dst_blend) = match &draw.draw_type {
                DrawType::Color => (&self.color_program, Gl::SRC_ALPHA, Gl::ONE_MINUS_SRC_ALPHA),
                DrawType::Gradient(_) => (
                    &self.gradient_program,
                    Gl::SRC_ALPHA,
                    Gl::ONE_MINUS_SRC_ALPHA,
                ),
                // Bitmaps use pre-multiplied alpha.
                DrawType::Bitmap { .. } => (&self.bitmap_program, Gl::ONE, Gl::ONE_MINUS_SRC_ALPHA),
            };

            // Set common render state, while minimizing unnecessary state changes.
            // TODO: Using designated layout specifiers in WebGL2/OpenGL ES 3, we could guarantee that uniforms
            // are in the same location between shaders, and avoid changing them unless necessary.
            if program as *const ShaderProgram != self.active_program {
                self.gl.use_program(Some(&program.program));
                self.active_program = program as *const ShaderProgram;

                program.uniform_matrix4fv(&self.gl, ShaderUniform::ViewMatrix, &self.view_matrix);

                self.mult_color = None;
                self.add_color = None;

                if (src_blend, dst_blend) != self.blend_func {
                    self.gl.blend_func(src_blend, dst_blend);
                    self.blend_func = (src_blend, dst_blend);
                }
            }

            program.uniform_matrix4fv(&self.gl, ShaderUniform::WorldMatrix, &world_matrix);
            if Some(mult_color) != self.mult_color {
                program.uniform4fv(&self.gl, ShaderUniform::MultColor, &mult_color);
                self.mult_color = Some(mult_color);
            }
            if Some(add_color) != self.add_color {
                program.uniform4fv(&self.gl, ShaderUniform::AddColor, &add_color);
                self.add_color = Some(add_color);
            }

            // Set shader specific uniforms.
            match &draw.draw_type {
                DrawType::Color => (),
                DrawType::Gradient(gradient) => {
                    program.uniform_matrix3fv(
                        &self.gl,
                        ShaderUniform::TextureMatrix,
                        &gradient.matrix,
                    );
                    program.uniform1i(
                        &self.gl,
                        ShaderUniform::GradientType,
                        gradient.gradient_type,
                    );
                    program.uniform1fv(&self.gl, ShaderUniform::GradientRatios, &gradient.ratios);
                    let colors = unsafe {
                        std::slice::from_raw_parts(
                            gradient.colors[0].as_ptr(),
                            MAX_GRADIENT_COLORS * 4,
                        )
                    };
                    program.uniform4fv(&self.gl, ShaderUniform::GradientColors, &colors);
                    program.uniform1i(
                        &self.gl,
                        ShaderUniform::GradientNumColors,
                        gradient.num_colors as i32,
                    );
                    program.uniform1i(
                        &self.gl,
                        ShaderUniform::GradientRepeatMode,
                        gradient.repeat_mode,
                    );
                    program.uniform1f(
                        &self.gl,
                        ShaderUniform::GradientFocalPoint,
                        gradient.focal_point,
                    );
                    program.uniform1i(
                        &self.gl,
                        ShaderUniform::GradientInterpolation,
                        (gradient.interpolation == swf::GradientInterpolation::LinearRGB) as i32,
                    );
                }
                DrawType::Bitmap(bitmap) => {
                    let texture = if let Some(texture) =
                        self.textures.iter().find(|(id, _tex)| *id == bitmap.id)
                    {
                        &texture.1
                    } else {
                        // Bitmap not registered
                        continue;
                    };

                    program.uniform_matrix3fv(
                        &self.gl,
                        ShaderUniform::TextureMatrix,
                        &bitmap.matrix,
                    );

                    // Bind texture.
                    self.gl.active_texture(Gl::TEXTURE0);
                    self.gl.bind_texture(Gl::TEXTURE_2D, Some(&texture.texture));
                    program.uniform1i(&self.gl, ShaderUniform::BitmapTexture, 0);

                    // Set texture parameters.
                    let filter = if bitmap.is_smoothed {
                        Gl::LINEAR as i32
                    } else {
                        Gl::NEAREST as i32
                    };
                    self.gl
                        .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, filter);
                    self.gl
                        .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, filter);
                    // On WebGL1, you are unable to change the wrapping parameter causes non-power-of-2 textures.
                    let wrap = if self.gl2.is_some() && bitmap.is_repeating {
                        Gl::MIRRORED_REPEAT as i32
                    } else {
                        Gl::CLAMP_TO_EDGE as i32
                    };
                    self.gl
                        .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, wrap);
                    self.gl
                        .tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, wrap);
                }
            }

            // Draw the triangles.
            self.gl
                .draw_elements_with_i32(Gl::TRIANGLES, draw.num_indices, Gl::UNSIGNED_SHORT, 0);
        }
    }

    fn draw_rect(&mut self, color: Color, matrix: &Matrix) {
        let world_matrix = [
            [matrix.a, matrix.b, 0.0, 0.0],
            [matrix.c, matrix.d, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [
                matrix.tx.to_pixels() as f32,
                matrix.ty.to_pixels() as f32,
                0.0,
                1.0,
            ],
        ];

        let mult_color = [1.0, 1.0, 1.0, 1.0];

        let add_color = [
            color.r as f32 * 255.0,
            color.g as f32 * 255.0,
            color.b as f32 * 255.0,
            color.a as f32 * 255.0,
        ];

        self.set_stencil_state();

        let program = &self.color_program;
        let src_blend = Gl::SRC_ALPHA;
        let dst_blend = Gl::ONE_MINUS_SRC_ALPHA;

        // Set common render state, while minimizing unnecessary state changes.
        // TODO: Using designated layout specifiers in WebGL2/OpenGL ES 3, we could guarantee that uniforms
        // are in the same location between shaders, and avoid changing them unless necessary.
        if program as *const ShaderProgram != self.active_program {
            self.gl.use_program(Some(&program.program));
            self.active_program = program as *const ShaderProgram;

            program.uniform_matrix4fv(&self.gl, ShaderUniform::ViewMatrix, &self.view_matrix);

            self.mult_color = None;
            self.add_color = None;

            if (src_blend, dst_blend) != self.blend_func {
                self.gl.blend_func(src_blend, dst_blend);
                self.blend_func = (src_blend, dst_blend);
            }
        };

        self.color_program
            .uniform_matrix4fv(&self.gl, ShaderUniform::WorldMatrix, &world_matrix);
        if Some(mult_color) != self.mult_color {
            self.color_program
                .uniform4fv(&self.gl, ShaderUniform::MultColor, &mult_color);
            self.mult_color = Some(mult_color);
        }
        if Some(add_color) != self.add_color {
            self.color_program
                .uniform4fv(&self.gl, ShaderUniform::AddColor, &add_color);
            self.add_color = Some(add_color);
        }

        let quad = &self.meshes[self.quad_shape.0];
        self.bind_vertex_array(Some(&quad.draws[0].vao));

        self.gl.draw_elements_with_i32(
            Gl::TRIANGLES,
            quad.draws[0].num_indices,
            Gl::UNSIGNED_SHORT,
            0,
        );
    }

    fn draw_letterbox(&mut self, letterbox: Letterbox) {
        self.set_stencil_state();

        self.gl.clear_color(0.0, 0.0, 0.0, 0.0);

        match letterbox {
            Letterbox::None => (),
            Letterbox::Letterbox(margin_height) => {
                self.gl.enable(Gl::SCISSOR_TEST);
                self.gl
                    .scissor(0, 0, self.viewport_width as i32, margin_height as i32);
                self.gl.clear(Gl::COLOR_BUFFER_BIT);
                self.gl.scissor(
                    0,
                    (self.viewport_height - margin_height) as i32,
                    self.viewport_width as i32,
                    margin_height as i32 + 1,
                );
                self.gl.clear(Gl::COLOR_BUFFER_BIT);
                self.gl.disable(Gl::SCISSOR_TEST);
            }
            Letterbox::Pillarbox(margin_width) => {
                self.gl.enable(Gl::SCISSOR_TEST);
                self.gl
                    .scissor(0, 0, margin_width as i32, self.viewport_height as i32);
                self.gl.clear(Gl::COLOR_BUFFER_BIT);
                self.gl.scissor(
                    (self.viewport_width - margin_width) as i32,
                    0,
                    margin_width as i32 + 1,
                    self.viewport_height as i32,
                );
                self.gl.clear(Gl::COLOR_BUFFER_BIT);
                self.gl.scissor(
                    0,
                    0,
                    self.viewport_width as i32,
                    self.viewport_height as i32,
                );
                self.gl.disable(Gl::SCISSOR_TEST);
            }
        }
    }

    fn push_mask(&mut self) {
        assert!(
            self.mask_state == MaskState::NoMask || self.mask_state == MaskState::DrawMaskedContent
        );
        self.num_masks += 1;
        self.mask_state = MaskState::DrawMaskStencil;
        self.mask_state_dirty = true;
    }

    fn activate_mask(&mut self) {
        assert!(self.num_masks > 0 && self.mask_state == MaskState::DrawMaskStencil);
        self.mask_state = MaskState::DrawMaskedContent;
        self.mask_state_dirty = true;
    }

    fn deactivate_mask(&mut self) {
        assert!(self.num_masks > 0 && self.mask_state == MaskState::DrawMaskedContent);
        self.mask_state = MaskState::ClearMaskStencil;
        self.mask_state_dirty = true;
    }

    fn pop_mask(&mut self) {
        assert!(self.num_masks > 0 && self.mask_state == MaskState::ClearMaskStencil);
        self.num_masks -= 1;
        self.mask_state = if self.num_masks == 0 {
            MaskState::NoMask
        } else {
            MaskState::DrawMaskedContent
        };
        self.mask_state_dirty = true;
    }
}

struct Texture {
    width: u32,
    height: u32,
    texture: WebGlTexture,
}

#[derive(Clone, Debug)]
struct Gradient {
    matrix: [[f32; 3]; 3],
    gradient_type: i32,
    ratios: [f32; MAX_GRADIENT_COLORS],
    colors: [[f32; 4]; MAX_GRADIENT_COLORS],
    num_colors: u32,
    repeat_mode: i32,
    focal_point: f32,
    interpolation: swf::GradientInterpolation,
}

#[derive(Clone, Debug)]
struct BitmapDraw {
    matrix: [[f32; 3]; 3],
    id: swf::CharacterId,
    is_repeating: bool,
    is_smoothed: bool,
}

struct Mesh {
    draws: Vec<Draw>,
}

#[allow(dead_code)]
struct Draw {
    draw_type: DrawType,
    vertex_buffer: WebGlBuffer,
    index_buffer: WebGlBuffer,
    vao: WebGlVertexArrayObject,
    num_indices: i32,
}

enum DrawType {
    Color,
    Gradient(Box<Gradient>),
    Bitmap(BitmapDraw),
}

struct MsaaBuffers {
    color_renderbuffer: WebGlRenderbuffer,
    stencil_renderbuffer: WebGlRenderbuffer,
    render_framebuffer: WebGlFramebuffer,
    color_framebuffer: WebGlFramebuffer,
    framebuffer_texture: WebGlTexture,
}

// Because the shaders are currently simple and few in number, we are using a
// straightforward shader model. We maintain an enum of every possible uniform,
// and each shader tries to grab the location of each uniform.
struct ShaderProgram {
    program: WebGlProgram,
    uniforms: [Option<WebGlUniformLocation>; NUM_UNIFORMS],
    vertex_position_location: u32,
    vertex_color_location: u32,
    num_vertex_attributes: u32,
}

// These should match the uniform names in the shaders.
const NUM_UNIFORMS: usize = 13;
const UNIFORM_NAMES: [&str; NUM_UNIFORMS] = [
    "world_matrix",
    "view_matrix",
    "mult_color",
    "add_color",
    "u_matrix",
    "u_gradient_type",
    "u_ratios",
    "u_colors",
    "u_num_colors",
    "u_repeat_mode",
    "u_focal_point",
    "u_interpolation",
    "u_texture",
];

enum ShaderUniform {
    WorldMatrix = 0,
    ViewMatrix,
    MultColor,
    AddColor,
    TextureMatrix,
    GradientType,
    GradientRatios,
    GradientColors,
    GradientNumColors,
    GradientRepeatMode,
    GradientFocalPoint,
    GradientInterpolation,
    BitmapTexture,
}

impl ShaderProgram {
    fn new(
        gl: &Gl,
        vertex_shader: &WebGlShader,
        fragment_shader: &WebGlShader,
    ) -> Result<Self, Error> {
        let program = gl.create_program().ok_or("Unable to create program")?;
        gl.attach_shader(&program, &vertex_shader);
        gl.attach_shader(&program, &fragment_shader);

        gl.link_program(&program);
        if !gl
            .get_program_parameter(&program, Gl::LINK_STATUS)
            .as_bool()
            .unwrap_or(false)
        {
            let msg = format!(
                "Error linking shader program: {:?}",
                gl.get_program_info_log(&program)
            );
            log::error!("{}", msg);
            return Err(msg.into());
        }

        // Find uniforms.
        let mut uniforms: [Option<WebGlUniformLocation>; NUM_UNIFORMS] = Default::default();
        for i in 0..NUM_UNIFORMS {
            uniforms[i] = gl.get_uniform_location(&program, UNIFORM_NAMES[i]);
        }

        let vertex_position_location = gl.get_attrib_location(&program, "position") as u32;
        let vertex_color_location = gl.get_attrib_location(&program, "color") as u32;
        let num_vertex_attributes = if vertex_position_location != 0xffff_ffff {
            1
        } else {
            0
        } + if vertex_color_location != 0xffff_ffff {
            1
        } else {
            0
        };

        Ok(ShaderProgram {
            program,
            uniforms,
            vertex_position_location,
            vertex_color_location,
            num_vertex_attributes,
        })
    }

    fn uniform1f(&self, gl: &Gl, uniform: ShaderUniform, value: f32) {
        gl.uniform1f(self.uniforms[uniform as usize].as_ref(), value);
    }

    fn uniform1fv(&self, gl: &Gl, uniform: ShaderUniform, values: &[f32]) {
        gl.uniform1fv_with_f32_array(self.uniforms[uniform as usize].as_ref(), values);
    }

    fn uniform1i(&self, gl: &Gl, uniform: ShaderUniform, value: i32) {
        gl.uniform1i(self.uniforms[uniform as usize].as_ref(), value);
    }

    fn uniform4fv(&self, gl: &Gl, uniform: ShaderUniform, values: &[f32]) {
        gl.uniform4fv_with_f32_array(self.uniforms[uniform as usize].as_ref(), values);
    }

    fn uniform_matrix3fv(&self, gl: &Gl, uniform: ShaderUniform, values: &[[f32; 3]; 3]) {
        gl.uniform_matrix3fv_with_f32_array(
            self.uniforms[uniform as usize].as_ref(),
            false,
            unsafe { std::slice::from_raw_parts(values[0].as_ptr(), 9) },
        );
    }

    fn uniform_matrix4fv(&self, gl: &Gl, uniform: ShaderUniform, values: &[[f32; 4]; 4]) {
        gl.uniform_matrix4fv_with_f32_array(
            self.uniforms[uniform as usize].as_ref(),
            false,
            unsafe { std::slice::from_raw_parts(values[0].as_ptr(), 16) },
        );
    }
}

impl WebGlRenderBackend {}

trait GlExt {
    fn check_error(&self, error_msg: &'static str) -> Result<(), Error>;
}

impl GlExt for Gl {
    /// Check if GL returned an error for the previous operation.
    fn check_error(&self, error_msg: &'static str) -> Result<(), Error> {
        match self.get_error() {
            Self::NO_ERROR => Ok(()),
            error => Err(format!("WebGL: Error in {}: {}", error_msg, error).into()),
        }
    }
}

impl GlExt for Gl2 {
    /// Check if GL returned an error for the previous operation.
    fn check_error(&self, error_msg: &'static str) -> Result<(), Error> {
        match self.get_error() {
            Self::NO_ERROR => Ok(()),
            error => Err(format!("WebGL: Error in {}: {}", error_msg, error).into()),
        }
    }
}
