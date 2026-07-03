// Link gate: exercises the headless world-step call graph so wasm-ld must
// resolve every symbol the real integration will need.
#include "box3d/box3d.h"

float b3stdb_smoke(int steps)
{
	b3WorldDef worldDef = b3DefaultWorldDef();
	b3WorldId world = b3CreateWorld(&worldDef);

	b3BodyDef bodyDef = b3DefaultBodyDef();
	bodyDef.type = b3_dynamicBody;
	bodyDef.position = (b3Vec3){ 0.0f, 0.0f, 10.0f };
	b3BodyId body = b3CreateBody(world, &bodyDef);

	b3ShapeDef shapeDef = b3DefaultShapeDef();
	b3Sphere sphere = { { 0.0f, 0.0f, 0.0f }, 0.5f };
	b3CreateSphereShape(body, &shapeDef, &sphere);

	for (int i = 0; i < steps; ++i)
	{
		b3World_Step(world, 1.0f / 60.0f, 4);
	}

	b3Pos p = b3Body_GetPosition(body);
	b3DestroyWorld(world);
	return p.z;
}
