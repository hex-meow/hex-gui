// 3D 数字孪生:three.js + urdf-loader 加载 URDF,按 joint_state 实时更新关节角。
// previewQ 非空时叠加一个半透明“幽灵臂”到目标位姿(预设悬浮预览,先看后动)。
// urdfXml 给了就用它(从机器人级 <prefix>/urdf 取的整机 arm+EE,或臂-only 回退);否则退到
// 捆在前端 public/urdf/ 的 firefly。整机时在装配面(link_6 / ee_base_link)画坐标轴,肉眼核对夹爪原点贴合法兰。
import { useEffect, useRef } from "react";
import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import { STLLoader } from "three/addons/loaders/STLLoader.js";
import URDFLoader from "urdf-loader";
import type { URDFRobot } from "urdf-loader";

interface Props {
  q: number[];
  gravity: [number, number, number];
  jointNames: string[];
  previewQ?: number[] | null; // 悬浮预设时的目标位姿(幽灵臂)
  armQuat?: [number, number, number, number] | null; // 整臂朝向(x,y,z,w);给了就直接用它转臂(四元数模式),否则从重力方向反推
  urdfXml?: string | null; // 机器人级 URDF(整机 arm+EE 或臂-only);给了就渲它,否则退到捆的 firefly
}

export function ArmViewer({ q, gravity, jointNames, previewQ, armQuat, urdfXml }: Props) {
  const mountRef = useRef<HTMLDivElement>(null);
  const robotRef = useRef<URDFRobot | null>(null);
  const ghostRef = useRef<URDFRobot | null>(null);
  const arrowRef = useRef<THREE.ArrowHelper | null>(null);
  const armRootRef = useRef<THREE.Group | null>(null); // 整臂根:改重力向量时旋转它(臂倾斜、重力始终朝下=人眼所见)
  const autoJointsRef = useRef<string[]>([]);

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

    let raf = 0;
    const animate = () => { controls.update(); renderer.render(scene, camera); raf = requestAnimationFrame(animate); };
    animate();
    const onResize = () => {
      const w = mount.clientWidth || 600;
      camera.aspect = w / H; camera.updateProjectionMatrix(); renderer.setSize(w, H);
    };
    window.addEventListener("resize", onResize);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", onResize);
      renderer.dispose();
      if (renderer.domElement.parentNode === mount) mount.removeChild(renderer.domElement);
    };
  }, []);

  // 加载 robot + ghost(urdfXml 变了就重载)。整机时在装配面画坐标轴核对夹爪原点。
  useEffect(() => {
    const armRoot = armRootRef.current;
    if (!armRoot) return;
    let cancelled = false;

    // 先拆旧 robot/ghost 释放几何/材质,避免泄漏(切臂/切装配态时)。
    const dispose = (obj: URDFRobot | null) => {
      if (!obj) return;
      armRoot.remove(obj);
      obj.traverse((o) => {
        const m = o as THREE.Mesh;
        if (m.isMesh) {
          m.geometry?.dispose();
          const mat = m.material;
          if (Array.isArray(mat)) mat.forEach((x) => x.dispose()); else mat?.dispose();
        }
      });
    };
    dispose(robotRef.current); robotRef.current = null;
    dispose(ghostRef.current); ghostRef.current = null;

    const loader = new URDFLoader();
    // package:// 解析:firefly(捆的)+ gp80 夹爪(整机 URDF 里 EE 网格用 hex_gp80_description)。
    loader.packages = { xpkg_urdf_firefly_y6: "/urdf", hex_gp80_description: "/urdf/gp80" };
    // ⚠️ 真实签名是 (path, manager, material, onComplete) —— 4 个参数(.d.ts 漏了 material)。
    (loader as any).loadMeshCb = (
      url: string,
      manager: THREE.LoadingManager,
      _material: THREE.Material,
      onComplete: (obj: THREE.Object3D | null, err?: Error) => void,
    ) => {
      new STLLoader(manager).load(
        url,
        (geom) => onComplete(new THREE.Mesh(geom, new THREE.MeshPhongMaterial({ color: 0xbfc4cc }))),
        undefined,
        (err) => onComplete(null, err as Error),
      );
    };

    // urdfXml 给了就用 loader.parse(公开方法,0.13 支持;package:// 仍走 loader.packages);否则退到捆的文件。
    const load = (): Promise<URDFRobot> =>
      urdfXml ? Promise.resolve(loader.parse(urdfXml)) : loader.loadAsync("/urdf/firefly.urdf");

    load().then((robot) => {
      if (cancelled) { dispose(robot); return; }
      robotRef.current = robot;
      autoJointsRef.current = Object.keys(robot.joints).filter((n) => (robot.joints[n] as any).jointType !== "fixed");
      armRoot.add(robot);
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
    }).catch((e) => console.error("URDF load failed", e));

    // 幽灵臂(预设预览):半透明绿色,默认隐藏。与主臂同源。
    load().then((ghost) => {
      if (cancelled) { dispose(ghost); return; }
      ghost.traverse((o) => {
        if ((o as THREE.Mesh).isMesh) {
          (o as THREE.Mesh).material = new THREE.MeshPhongMaterial({ color: 0x44dd88, transparent: true, opacity: 0.35, depthWrite: false });
        }
      });
      ghost.visible = false;
      ghostRef.current = ghost;
      armRoot.add(ghost);
    }).catch(() => {});

    return () => { cancelled = true; };
  }, [urdfXml]);

  // 实时关节角
  useEffect(() => {
    const robot = robotRef.current;
    if (!robot) return;
    const names = jointNames.length ? jointNames : autoJointsRef.current;
    names.forEach((n, i) => { if (robot.joints[n]) robot.setJointValue(n, q[i] ?? 0); });
  }, [q, jointNames]);

  // 幽灵臂:预设悬浮预览
  useEffect(() => {
    const ghost = ghostRef.current;
    if (!ghost) return;
    if (previewQ && previewQ.length) {
      const names = jointNames.length ? jointNames : autoJointsRef.current;
      names.forEach((n, i) => { if (ghost.joints[n]) ghost.setJointValue(n, previewQ[i] ?? 0); });
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

  return <div ref={mountRef} style={{ width: "100%", height: 440, borderRadius: 8, overflow: "hidden" }} />;
}
