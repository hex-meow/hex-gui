// 3D 数字孪生:three.js + urdf-loader 加载 URDF,按 joint_state 实时更新关节角。
// previewQ 非空时叠加一个半透明“幽灵臂”到目标位姿(预设悬浮预览,先看后动)。
// urdfXml 给了就用它(从机器人级 <prefix>/urdf 取的整机 arm+EE,或臂-only 回退);否则退到
// 捆在前端 public/urdf/ 的 firefly。整机时在装配面(link_6 / ee_base_link)画坐标轴,肉眼核对夹爪原点贴合法兰。
import { useEffect, useRef, useState } from "react";
import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import type { URDFRobot } from "urdf-loader";
import {
  beginUrdfLoad,
  DEFAULT_ARM_URDF_URL,
  disposeObject3D,
  isUrdfAbort,
  type UrdfLoadJob,
} from "../urdfModelLoader";

interface Props {
  q: number[];
  gravity: [number, number, number];
  jointNames: string[];
  previewQ?: number[] | null; // 悬浮预设时的目标位姿(幽灵臂)
  armQuat?: [number, number, number, number] | null; // 整臂朝向(x,y,z,w);给了就直接用它转臂(四元数模式),否则从重力方向反推
  urdfXml?: string | null; // 机器人级 URDF(整机 arm+EE 或臂-only);给了就渲它,否则退到捆的 firefly
}

function createArmPlaceholder(): THREE.Group {
  const group = new THREE.Group();
  group.name = "arm-loading-placeholder";
  const material = new THREE.MeshPhongMaterial({
    color: 0x718096,
    wireframe: true,
    transparent: true,
    opacity: 0.5,
  });
  const addPart = (geometry: THREE.BufferGeometry, position: [number, number, number], rotationY = 0) => {
    const mesh = new THREE.Mesh(geometry, material);
    mesh.position.set(...position);
    mesh.rotation.y = rotationY;
    group.add(mesh);
  };
  const baseGeometry = new THREE.CylinderGeometry(0.12, 0.14, 0.07, 20);
  baseGeometry.rotateX(Math.PI / 2);
  addPart(baseGeometry, [0, 0, 0.035]);
  addPart(new THREE.BoxGeometry(0.07, 0.08, 0.28), [0, 0, 0.2]);
  addPart(new THREE.BoxGeometry(0.07, 0.07, 0.3), [0.1, 0, 0.45], Math.PI / 4);
  addPart(new THREE.SphereGeometry(0.065, 16, 10), [0.205, 0, 0.555]);
  return group;
}

function automaticJointNames(robot: URDFRobot): string[] {
  return Object.keys(robot.joints).filter((name) => (robot.joints[name] as { jointType?: string }).jointType !== "fixed");
}

function applyJointValues(robot: URDFRobot, values: readonly number[], jointNames: readonly string[], automaticNames: readonly string[]) {
  const names = jointNames.length ? jointNames : automaticNames;
  names.forEach((name, index) => {
    if (robot.joints[name]) robot.setJointValue(name, values[index] ?? 0);
  });
}

function makeGhost(robot: URDFRobot): URDFRobot {
  const ghost = robot.clone(true) as URDFRobot;
  ghost.traverse((object) => {
    const mesh = object as THREE.Mesh;
    if (!mesh.isMesh) return;
    const configure = (source: THREE.Material) => {
      const material = source.clone();
      const colorMaterial = material as THREE.Material & { color?: THREE.Color };
      colorMaterial.color?.setHex(0x44dd88);
      material.transparent = true;
      material.opacity = 0.35;
      material.depthWrite = false;
      return material;
    };
    mesh.material = Array.isArray(mesh.material)
      ? mesh.material.map(configure)
      : configure(mesh.material);
  });
  return ghost;
}

function disposeArmModels(robot: URDFRobot | null, ghost: URDFRobot | null) {
  // AxesHelper 不是 Mesh，通用 disposeObject3D 不会处理它的 line geometry/material。
  robot?.traverse((object) => {
    if (object instanceof THREE.AxesHelper) object.dispose();
  });
  disposeObject3D(robot, ghost);
}

function loadErrorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try { return JSON.stringify(error); } catch { return String(error); }
}

export function ArmViewer({ q, gravity, jointNames, previewQ, armQuat, urdfXml }: Props) {
  const mountRef = useRef<HTMLDivElement>(null);
  const robotRef = useRef<URDFRobot | null>(null);
  const ghostRef = useRef<URDFRobot | null>(null);
  const placeholderRef = useRef<THREE.Group | null>(null);
  const arrowRef = useRef<THREE.ArrowHelper | null>(null);
  const armRootRef = useRef<THREE.Group | null>(null); // 整臂根:改重力向量时旋转它(臂倾斜、重力始终朝下=人眼所见)
  const autoJointsRef = useRef<string[]>([]);
  const loadGenerationRef = useRef(0);
  const loadJobRef = useRef<UrdfLoadJob | null>(null);
  const latestQRef = useRef(q);
  const latestJointNamesRef = useRef(jointNames);
  const latestPreviewQRef = useRef(previewQ);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [retryGeneration, setRetryGeneration] = useState(0);

  latestQRef.current = q;
  latestJointNamesRef.current = jointNames;
  latestPreviewQRef.current = previewQ;

  useEffect(() => {
    const mount = mountRef.current!;
    const H = 440;
    const W = mount.clientWidth || 600;
    const scene = new THREE.Scene();
    scene.background = new THREE.Color(0x1a1d23);
    const camera = new THREE.PerspectiveCamera(50, W / H, 0.01, 100);
    camera.position.set(0.7, -0.9, 0.7);
    camera.up.set(0, 0, 1); // URDF 是 Z-up
    const renderer = new THREE.WebGLRenderer({ antialias: true });
    renderer.setSize(W, H);
    renderer.setPixelRatio(window.devicePixelRatio);
    mount.appendChild(renderer.domElement);
    const controls = new OrbitControls(camera, renderer.domElement);
    controls.target.set(0, 0, 0); // 始终绕零点(基座原点)旋转/缩放
    controls.enablePan = false;   // 禁止平移 → 只能绕零点 orbit + zoom

    scene.add(new THREE.AmbientLight(0xffffff, 0.75));
    const dir = new THREE.DirectionalLight(0xffffff, 0.8);
    dir.position.set(1, 1, 2);
    scene.add(dir);
    const grid = new THREE.GridHelper(2, 20, 0x444444, 0x2a2a2a).rotateX(Math.PI / 2); // XY 平面(Z-up)
    (grid.material as THREE.Material).transparent = true;
    (grid.material as THREE.Material).opacity = 0.3;
    scene.add(grid);

    // 重力箭头:放在地板下方,且 depthTest=false → 不被机械臂/地板遮挡,始终可见。
    const arrow = new THREE.ArrowHelper(new THREE.Vector3(0, 0, -1), new THREE.Vector3(0, 0, -0.04), 0.34, 0xff5555, 0.08, 0.05);
    arrow.position.set(0, 0, -0.04);
    [arrow.line.material, arrow.cone.material].forEach((m) => { (m as THREE.Material).depthTest = false; (m as THREE.Material).transparent = true; });
    arrow.renderOrder = 999;
    scene.add(arrow);
    arrowRef.current = arrow;

    // 整臂根:robot/ghost 挂在它下面;改重力时旋转它(地板/箭头留在 world,臂随重力倾斜)。
    const armRoot = new THREE.Group();
    scene.add(armRoot);
    armRootRef.current = armRoot;
    const placeholder = createArmPlaceholder();
    armRoot.add(placeholder);
    placeholderRef.current = placeholder;

    let raf = 0;
    const animate = () => { controls.update(); renderer.render(scene, camera); raf = requestAnimationFrame(animate); };
    animate();
    const onResize = () => {
      const w = mount.clientWidth || 600;
      camera.aspect = w / H; camera.updateProjectionMatrix(); renderer.setSize(w, H);
    };
    window.addEventListener("resize", onResize);
    return () => {
      loadGenerationRef.current += 1;
      loadJobRef.current?.cancel();
      loadJobRef.current = null;
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", onResize);
      controls.dispose();
      disposeArmModels(robotRef.current, ghostRef.current);
      robotRef.current = null;
      ghostRef.current = null;
      disposeObject3D(placeholderRef.current);
      placeholderRef.current = null;
      autoJointsRef.current = [];
      arrow.dispose();
      grid.dispose();
      arrowRef.current = null;
      armRootRef.current = null;
      renderer.dispose();
      if (renderer.domElement.parentNode === mount) mount.removeChild(renderer.domElement);
    };
  }, []);

  // 先在场景外等待 URDF 和全部 STL；只有候选模型完整成功后才原子替换。
  useEffect(() => {
    const armRoot = armRootRef.current;
    if (!armRoot) return;
    let active = true;
    const generation = ++loadGenerationRef.current;
    setLoading(true);
    setLoadError(null);

    const job = beginUrdfLoad({
      source: urdfXml
        ? { kind: "xml", xml: urdfXml, label: "controller://inline-robot.urdf" }
        : { kind: "url", url: DEFAULT_ARM_URDF_URL },
    });
    loadJobRef.current = job;

    job.promise.then(({ robot }) => {
      let ghost: URDFRobot | null = null;
      let automaticNames: string[];
      try {
        ghost = makeGhost(robot);
        automaticNames = automaticJointNames(robot);
        applyJointValues(robot, latestQRef.current, latestJointNamesRef.current, automaticNames);
        const target = latestPreviewQRef.current;
        if (target?.length) {
          applyJointValues(ghost, target, latestJointNamesRef.current, automaticNames);
          ghost.visible = true;
        } else {
          ghost.visible = false;
        }

        // 装配面坐标轴:核对夹爪 base_link 原点是否贴在臂法兰。attach 到臂 tip(link_6)+ EE 根(ee_base_link,整机才有)。
        // depthTest=false + 高 renderOrder → 不被网格遮挡,始终可见(仿重力箭头)。仅主臂加,不加幽灵臂。
        for (const link of ["link_6", "ee_base_link"]) {
          const obj = robot.links[link];
          if (!obj) continue;
          const axes = new THREE.AxesHelper(0.06);
          axes.renderOrder = 998;
          (axes.material as THREE.Material).depthTest = false;
          (axes.material as THREE.Material).transparent = true;
          obj.add(axes);
        }
      } catch (error) {
        disposeArmModels(robot, ghost);
        throw error;
      }

      if (!active || generation !== loadGenerationRef.current || armRootRef.current !== armRoot) {
        disposeArmModels(robot, ghost);
        return;
      }

      const oldRobot = robotRef.current;
      const oldGhost = ghostRef.current;
      armRoot.add(robot, ghost);
      robotRef.current = robot;
      ghostRef.current = ghost;
      autoJointsRef.current = automaticNames;

      if (placeholderRef.current) {
        disposeObject3D(placeholderRef.current);
        placeholderRef.current = null;
      }
      disposeArmModels(oldRobot, oldGhost);
      setLoadError(null);
      setLoading(false);
    }).catch((error: unknown) => {
      if (!active || generation !== loadGenerationRef.current || isUrdfAbort(error)) return;
      console.error("URDF load failed", error);
      setLoadError(loadErrorMessage(error));
      setLoading(false);
    }).finally(() => {
      if (loadJobRef.current === job) loadJobRef.current = null;
    });

    return () => {
      active = false;
      job.cancel();
      if (loadJobRef.current === job) loadJobRef.current = null;
    };
  }, [urdfXml, retryGeneration]);

  // 实时关节角
  useEffect(() => {
    const robot = robotRef.current;
    if (!robot) return;
    applyJointValues(robot, q, jointNames, autoJointsRef.current);
  }, [q, jointNames]);

  // 幽灵臂:预设悬浮预览
  useEffect(() => {
    const ghost = ghostRef.current;
    if (!ghost) return;
    if (previewQ && previewQ.length) {
      applyJointValues(ghost, previewQ, jointNames, autoJointsRef.current);
      ghost.visible = true;
    } else {
      ghost.visible = false;
    }
  }, [previewQ, jointNames]);

  // 重力可视化:箭头**始终朝下**(world -Z);**旋转整臂**让人看到机械臂在真实空间里的样子。
  // armQuat 给了(四元数模式)→ 直接用它当整臂朝向(无歧义);否则从重力方向反推最小旋转(XYZ 模式)。
  useEffect(() => {
    const g = new THREE.Vector3(gravity[0], gravity[1], gravity[2]);
    const len = g.length();
    const arrow = arrowRef.current;
    if (arrow && len > 1e-6) {
      arrow.setDirection(new THREE.Vector3(0, 0, -1)); // 固定朝下
      arrow.setLength(0.12 + 0.22 * Math.min(len / 9.81, 1), 0.05, 0.03); // 长度示意大小
    }
    const armRoot = armRootRef.current;
    if (armRoot) {
      if (armQuat) {
        armRoot.quaternion.set(armQuat[0], armQuat[1], armQuat[2], armQuat[3]).normalize();
      } else if (len > 1e-6) {
        armRoot.quaternion.setFromUnitVectors(g.clone().normalize(), new THREE.Vector3(0, 0, -1));
      }
    }
  }, [gravity, armQuat]);

  return (
    <div style={{ position: "relative", width: "100%", height: 440, borderRadius: 8, overflow: "hidden" }}>
      <div ref={mountRef} style={{ position: "absolute", inset: 0 }} />
      {loadError && (
        <div
          role="alert"
          style={{
            position: "absolute",
            left: 12,
            right: 12,
            bottom: 12,
            zIndex: 2,
            padding: "10px 12px",
            border: "1px solid rgba(255, 99, 99, 0.7)",
            borderRadius: 6,
            background: "rgba(43, 17, 20, 0.94)",
            color: "#ffd7d7",
            fontSize: 12,
            boxShadow: "0 4px 14px rgba(0, 0, 0, 0.35)",
          }}
        >
          <div style={{ fontWeight: 600, marginBottom: 6 }}>
            {robotRef.current
              ? "3D 模型加载失败（继续显示上一版可用模型）"
              : "3D 模型加载失败（已保留线框占位，不显示透明骨架）"}
          </div>
          <div style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere", maxHeight: 130, overflowY: "auto" }}>
            {loadError}
          </div>
          <button
            type="button"
            disabled={loading}
            onClick={() => setRetryGeneration((value) => value + 1)}
            style={{ marginTop: 8, padding: "4px 10px", cursor: loading ? "wait" : "pointer" }}
          >
            {loading ? "重新加载中…" : "重试"}
          </button>
        </div>
      )}
    </div>
  );
}
